/* The libavcodec side of pf_libav.h. Decoders and the encoder run single threaded
 * (thread_count 1, no frame or slice threads), the deterministic simulation rule, unless a
 * video decoder is opened with frame threads. Nothing logs. */
#include "config.h"

#include <limits.h>
#include <pthread.h>
#include <stdio.h>
#include <string.h>

#include "libavcodec/avcodec.h"
#include "libavutil/buffer.h"
#include "libavutil/channel_layout.h"
#include "libavutil/error.h"
#include "libavutil/ffversion.h"
#include "libavutil/frame.h"
#include "libavutil/imgutils.h"
#include "libavutil/log.h"
#include "libavutil/mastering_display_metadata.h"
#include "libavutil/mem.h"
#include "libavutil/opt.h"
#include "libavutil/pixdesc.h"

#include "pf_libav.h"

_Static_assert(sizeof(pf_lav_frame) == 264, "pf_lav_frame layout");
_Static_assert(offsetof(pf_lav_frame, in_slot) == 136, "pf_lav_frame.in_slot");
_Static_assert(offsetof(pf_lav_frame, primaries) == 184, "pf_lav_frame.primaries");
_Static_assert(offsetof(pf_lav_frame, mastering_primaries) == 204, "pf_lav_frame.mastering");
_Static_assert(offsetof(pf_lav_frame, channel_mask) == 248, "pf_lav_frame.channel_mask");
_Static_assert(offsetof(pf_lav_frame, chroma_location) == 256, "pf_lav_frame.chroma_location");
_Static_assert(offsetof(pf_lav_frame, skip) == 260, "pf_lav_frame.skip");
_Static_assert(sizeof(pf_lav_slots) == 24, "pf_lav_slots layout");

/* libavcodec/internal.h, STRIDE_ALIGN, from the same configuration. */
#if HAVE_SIMD_ALIGN_64
#define PF_STRIDE_ALIGN 64
#elif HAVE_SIMD_ALIGN_32
#define PF_STRIDE_ALIGN 32
#elif HAVE_SIMD_ALIGN_16
#define PF_STRIDE_ALIGN 16
#else
#define PF_STRIDE_ALIGN 8
#endif

struct pf_lav_dec {
    AVCodecContext *ctx;
    AVCodecParserContext *parser;
    /* What the parser writes the stream's parameters into, kept off the decoder's. */
    AVCodecContext *pctx;
    AVPacket *pkt;
    AVFrame *frame;
    /* Padded packet memory, one pool of the largest size seen. */
    AVBufferPool *pkt_pool;
    size_t pkt_pool_size;
    /* Parser input with the zeroed padding libavcodec's parsers read past the end into. */
    uint8_t *parse_buf;
    unsigned parse_cap;
    pf_lav_slots slots;
    uint64_t fallbacks;
    int codec;
};

static pthread_once_t log_once = PTHREAD_ONCE_INIT;

static void quiet(void) { av_log_set_level(AV_LOG_QUIET); }

static enum AVCodecID codec_id(int codec)
{
    switch (codec) {
    case PF_LAV_H264: return AV_CODEC_ID_H264;
    case PF_LAV_HEVC: return AV_CODEC_ID_HEVC;
    case PF_LAV_VP8: return AV_CODEC_ID_VP8;
    case PF_LAV_VP9: return AV_CODEC_ID_VP9;
    case PF_LAV_AAC: return AV_CODEC_ID_AAC;
    case PF_LAV_AC3: return AV_CODEC_ID_AC3;
    case PF_LAV_EAC3: return AV_CODEC_ID_EAC3;
    case PF_LAV_MP3: return AV_CODEC_ID_MP3;
    case PF_LAV_DTS: return AV_CODEC_ID_DTS;
    case PF_LAV_TRUEHD: return AV_CODEC_ID_TRUEHD;
    case PF_LAV_MLP: return AV_CODEC_ID_MLP;
    case PF_LAV_MPEG2: return AV_CODEC_ID_MPEG2VIDEO;
    case PF_LAV_VORBIS: return AV_CODEC_ID_VORBIS;
    case PF_LAV_MPEG4: return AV_CODEC_ID_MPEG4;
    case PF_LAV_ALAC: return AV_CODEC_ID_ALAC;
    case PF_LAV_FLV1: return AV_CODEC_ID_FLV1;
    case PF_LAV_VP6: return AV_CODEC_ID_VP6;
    case PF_LAV_VP6F: return AV_CODEC_ID_VP6F;
    case PF_LAV_VP6A: return AV_CODEC_ID_VP6A;
    case PF_LAV_VP7: return AV_CODEC_ID_VP7;
    case PF_LAV_THEORA: return AV_CODEC_ID_THEORA;
    case PF_LAV_SPEEX: return AV_CODEC_ID_SPEEX;
    default: return AV_CODEC_ID_NONE;
    }
}

static size_t align_up(size_t v, size_t a) { return (v + a - 1) & ~(a - 1); }

/* The layout libavcodec's default allocator gives a picture (libavcodec/get_buffer.c,
 * update_frame_pool and video_get_buffer), the planes one after another in one host
 * slot instead of one pool buffer each, each padded the same. */
static int get_buffer2(AVCodecContext *avctx, AVFrame *frame, int flags)
{
    pf_lav_dec *d = avctx->opaque;
    if (avctx->codec_type != AVMEDIA_TYPE_VIDEO || !d || !d->slots.acquire)
        return avcodec_default_get_buffer2(avctx, frame, flags);

    int w = frame->width, h = frame->height;
    int stride_align[AV_NUM_DATA_POINTERS];
    int linesize[4];
    int unaligned;
    avcodec_align_dimensions2(avctx, &w, &h, stride_align);
    do {
        if (av_image_fill_linesizes(linesize, frame->format, w) < 0)
            return AVERROR(EINVAL);
        w += w & ~(w - 1);
        unaligned = 0;
        for (int i = 0; i < 4; i++)
            unaligned |= linesize[i] % stride_align[i];
    } while (unaligned);

    ptrdiff_t lines[4];
    size_t size[4];
    for (int i = 0; i < 4; i++)
        lines[i] = linesize[i];
    if (av_image_fill_plane_sizes(size, frame->format, h, lines) < 0)
        return AVERROR(EINVAL);

    size_t offset[4] = { 0 };
    size_t total = 0;
    for (int i = 0; i < 4; i++) {
        if (!size[i])
            continue;
        if (size[i] > INT_MAX - (16 + PF_STRIDE_ALIGN - 1))
            return AVERROR(EINVAL);
        offset[i] = total;
        total += align_up(size[i] + 16 + PF_STRIDE_ALIGN - 1, PF_STRIDE_ALIGN);
    }

    uint8_t *slot = d->slots.acquire(d->slots.opaque, total + PF_STRIDE_ALIGN);
    if (!slot) {
        d->fallbacks++;
        return avcodec_default_get_buffer2(avctx, frame, flags);
    }
    uint8_t *base = (uint8_t *)align_up((size_t)slot, PF_STRIDE_ALIGN);
    /* The host's own release and opaque, so a frame may outlive the decoder. */
    frame->buf[0] = av_buffer_create(base, total, d->slots.release, d->slots.opaque, 0);
    if (!frame->buf[0]) {
        d->slots.release(d->slots.opaque, base);
        return AVERROR(ENOMEM);
    }
    memset(frame->data, 0, sizeof(frame->data));
    memset(frame->linesize, 0, sizeof(frame->linesize));
    for (int i = 0; i < 4; i++) {
        if (!size[i])
            continue;
        frame->data[i] = base + offset[i];
        frame->linesize[i] = linesize[i];
    }
    frame->extended_data = frame->data;
    return 0;
}

int pf_lav_open(pf_lav_dec **out, int codec, const uint8_t *extradata, size_t extradata_len,
                const pf_lav_slots *slots, int parse, int threads, int reorder)
{
    *out = NULL;
    pthread_once(&log_once, quiet);
    enum AVCodecID id = codec_id(codec);
    const AVCodec *c = avcodec_find_decoder(id);
    if (!c)
        return PF_LAV_ERR_NO_CODEC;
    if (extradata_len > INT_MAX - AV_INPUT_BUFFER_PADDING_SIZE || threads < 1)
        return PF_LAV_ERR_ARG;
    if (threads > PF_LAV_MAX_THREADS)
        threads = PF_LAV_MAX_THREADS;
    /* Frame threads call get_buffer2 on their own threads, also after the host's call has
     * returned, so a threaded decoder never reaches the host's slots. Its pictures stay in
     * libavcodec's memory and come out with in_slot 0. */
    int threaded = threads > 1 && c->type == AVMEDIA_TYPE_VIDEO
                   && (c->capabilities & AV_CODEC_CAP_FRAME_THREADS);

    pf_lav_dec *d = av_mallocz(sizeof(*d));
    if (!d)
        return PF_LAV_ERR_NOMEM;
    d->codec = codec;
    if (slots && !threaded)
        d->slots = *slots;
    d->ctx = avcodec_alloc_context3(c);
    d->pkt = av_packet_alloc();
    d->frame = av_frame_alloc();
    if (!d->ctx || !d->pkt || !d->frame)
        goto nomem;
    if (parse) {
        d->parser = av_parser_init(id);
        d->pctx = avcodec_alloc_context3(NULL);
        if (!d->parser || !d->pctx) {
            int rc = d->pctx ? PF_LAV_ERR_NO_CODEC : PF_LAV_ERR_NOMEM;
            pf_lav_close(d);
            return rc;
        }
        d->pctx->codec_id = id;
    }

    AVCodecContext *ctx = d->ctx;
    ctx->opaque = d;
    ctx->thread_count = threaded ? threads : 1;
    ctx->thread_type = threaded ? FF_THREAD_FRAME : 0;
    ctx->pkt_timebase = (AVRational){ 1, 1000000000 };
    /* 8192x4352, the largest picture of any H.264, H.265 or AV1 level (H.264 Table A-1
     * MaxFS 139264 macroblocks at level 6.2). A header claiming more is refused before
     * a buffer is asked for. */
    ctx->max_pixels = 8192 * 4352;
    /* Vorbis drops its first frame behind an open or a flush, which the host counts to
     * place what follows, so the frame comes out whole with the count beside it. */
    if (id == AV_CODEC_ID_VORBIS)
        ctx->flags2 |= AV_CODEC_FLAG2_SKIP_MANUAL;
    /* The starting reorder delay, as the ffmpeg tool hands over its probe's (codec_par.c
     * video_delay). H.264 without VUI bitstream_restriction otherwise starts at 0 and grows on
     * the first B picture, dropping the pictures already out of order behind the first key
     * (h264_slice.c h264_select_output_frame, "no picture ooo"). Frame threads copy it at open. */
    if (c->type == AVMEDIA_TYPE_VIDEO && reorder >= 0)
        ctx->has_b_frames = reorder > PF_LAV_MAX_REORDER ? PF_LAV_MAX_REORDER : reorder;
    if (c->type == AVMEDIA_TYPE_VIDEO && d->slots.acquire)
        ctx->get_buffer2 = get_buffer2;
    /* Crops left by the exact amount, else av_frame_apply_cropping rounds crop_left down to
     * keep the planes aligned, to 0 for a small crop, and junk columns show. The planes are
     * read through their pointers, not assumed aligned. */
    if (c->type == AVMEDIA_TYPE_VIDEO)
        ctx->flags |= AV_CODEC_FLAG_UNALIGNED;
    if (extradata && extradata_len) {
        ctx->extradata = av_mallocz(extradata_len + AV_INPUT_BUFFER_PADDING_SIZE);
        if (!ctx->extradata)
            goto nomem;
        memcpy(ctx->extradata, extradata, extradata_len);
        ctx->extradata_size = (int)extradata_len;
    }
    int ret = avcodec_open2(ctx, c, NULL);
    if (ret < 0) {
        pf_lav_close(d);
        return ret;
    }
    *out = d;
    return PF_LAV_OK;
nomem:
    pf_lav_close(d);
    return PF_LAV_ERR_NOMEM;
}

void pf_lav_close(pf_lav_dec *d)
{
    if (!d)
        return;
    av_frame_free(&d->frame);
    av_packet_free(&d->pkt);
    if (d->parser)
        av_parser_close(d->parser);
    avcodec_free_context(&d->pctx);
    /* Frees every picture libavcodec still holds, releasing their slots. */
    avcodec_free_context(&d->ctx);
    av_buffer_pool_uninit(&d->pkt_pool);
    av_freep(&d->parse_buf);
    av_free(d);
}

int pf_lav_send(pf_lav_dec *d, const uint8_t *data, size_t len, int64_t pts, int64_t dts,
                int64_t duration)
{
    if (!data || !len || len > INT_MAX - AV_INPUT_BUFFER_PADDING_SIZE)
        return PF_LAV_ERR_ARG;
    size_t need = len + AV_INPUT_BUFFER_PADDING_SIZE;
    if (need > d->pkt_pool_size) {
        /* Grows in 64 KiB steps, the buffers out are freed as they return. */
        size_t size = align_up(need, 65536);
        av_buffer_pool_uninit(&d->pkt_pool);
        d->pkt_pool = av_buffer_pool_init(size, NULL);
        d->pkt_pool_size = d->pkt_pool ? size : 0;
        if (!d->pkt_pool)
            return PF_LAV_ERR_NOMEM;
    }
    AVPacket *pkt = d->pkt;
    pkt->buf = av_buffer_pool_get(d->pkt_pool);
    if (!pkt->buf)
        return PF_LAV_ERR_NOMEM;
    memcpy(pkt->buf->data, data, len);
    memset(pkt->buf->data + len, 0, AV_INPUT_BUFFER_PADDING_SIZE);
    pkt->data = pkt->buf->data;
    pkt->size = (int)len;
    pkt->pts = pts;
    pkt->dts = dts;
    /* libavcodec takes 0 for unknown, pf_lav_receive gives it back as INT64_MIN. */
    pkt->duration = duration < 0 ? 0 : duration;
    int ret = avcodec_send_packet(d->ctx, pkt);
    av_packet_unref(pkt);
    if (ret == AVERROR(EAGAIN))
        return PF_LAV_AGAIN;
    if (ret == AVERROR_EOF)
        return PF_LAV_EOF;
    return ret < 0 ? ret : PF_LAV_OK;
}

int pf_lav_drain(pf_lav_dec *d)
{
    int ret = avcodec_send_packet(d->ctx, NULL);
    if (ret == AVERROR_EOF)
        return PF_LAV_EOF;
    return ret < 0 ? ret : PF_LAV_OK;
}

static int pixfmt_of(int format, int *depth, int *sw, int *sh, int *range)
{
    const AVPixFmtDescriptor *desc = av_pix_fmt_desc_get(format);
    *depth = desc ? desc->comp[0].depth : 0;
    *sw = desc ? desc->log2_chroma_w : 0;
    *sh = desc ? desc->log2_chroma_h : 0;
    switch (format) {
    case AV_PIX_FMT_YUV420P: return PF_LAV_PIX_I420;
    /* VP6A, its alpha plane behind the three of I420 is left out. */
    case AV_PIX_FMT_YUVA420P: return PF_LAV_PIX_I420;
    case AV_PIX_FMT_YUVJ420P: *range = AVCOL_RANGE_JPEG; return PF_LAV_PIX_I420;
    case AV_PIX_FMT_GRAY8: return PF_LAV_PIX_GRAY8;
    case AV_PIX_FMT_YUV420P10LE: return PF_LAV_PIX_I420_10;
    default: break;
    }
    /* Planar YUV 4:2:2 and 4:4:4 and depths past 10. Not RGB, so no gbrp. */
    int unfit = AV_PIX_FMT_FLAG_BE | AV_PIX_FMT_FLAG_PAL | AV_PIX_FMT_FLAG_BITSTREAM | AV_PIX_FMT_FLAG_HWACCEL
                | AV_PIX_FMT_FLAG_RGB | AV_PIX_FMT_FLAG_ALPHA | AV_PIX_FMT_FLAG_BAYER | AV_PIX_FMT_FLAG_FLOAT
                | AV_PIX_FMT_FLAG_XYZ;
    if (!desc || desc->nb_components != 3 || !(desc->flags & AV_PIX_FMT_FLAG_PLANAR) || (desc->flags & unfit))
        return PF_LAV_PIX_OTHER;
    for (int i = 0; i < 3; i++) {
        const AVComponentDescriptor *c = &desc->comp[i];
        if (c->plane != i || c->offset != 0 || c->shift != 0 || c->depth != *depth || c->step != (*depth > 8 ? 2 : 1))
            return PF_LAV_PIX_OTHER;
    }
    if (*depth < 8 || *depth > 16)
        return PF_LAV_PIX_OTHER;
    if (format == AV_PIX_FMT_YUVJ422P || format == AV_PIX_FMT_YUVJ444P || format == AV_PIX_FMT_YUVJ440P)
        *range = AVCOL_RANGE_JPEG;
    return PF_LAV_PIX_YUV;
}

static int sample_fmt_of(int format)
{
    int planar = av_sample_fmt_is_planar(format) ? PF_LAV_PLANAR : 0;
    switch (av_get_packed_sample_fmt(format)) {
    case AV_SAMPLE_FMT_U8: return PF_LAV_SMP_U8 | planar;
    case AV_SAMPLE_FMT_S16: return PF_LAV_SMP_S16 | planar;
    case AV_SAMPLE_FMT_S32: return PF_LAV_SMP_S32 | planar;
    case AV_SAMPLE_FMT_FLT: return PF_LAV_SMP_FLT | planar;
    case AV_SAMPLE_FMT_DBL: return PF_LAV_SMP_DBL | planar;
    default: return PF_LAV_SMP_OTHER;
    }
}

/* A rational in units of 1 / scale, rounded to nearest, clamped to the field. */
static uint32_t scaled(AVRational q, int64_t scale, uint32_t most)
{
    if (q.den <= 0 || q.num < 0)
        return 0;
    int64_t v = ((int64_t)q.num * scale + q.den / 2) / q.den;
    return v > most ? most : (uint32_t)v;
}

static void side_data(const AVFrame *src, pf_lav_frame *f)
{
    const AVFrameSideData *sd = av_frame_get_side_data(src, AV_FRAME_DATA_MASTERING_DISPLAY_METADATA);
    if (sd) {
        const AVMasteringDisplayMetadata *m = (const void *)sd->data;
        if (m->has_primaries) {
            f->flags |= PF_LAV_FRAME_MASTERING;
            for (int i = 0; i < 3; i++) {
                f->mastering_primaries[2 * i] = scaled(m->display_primaries[i][0], 50000, 0xffff);
                f->mastering_primaries[2 * i + 1] = scaled(m->display_primaries[i][1], 50000, 0xffff);
            }
            f->mastering_white[0] = scaled(m->white_point[0], 50000, 0xffff);
            f->mastering_white[1] = scaled(m->white_point[1], 50000, 0xffff);
        }
        if (m->has_luminance) {
            f->flags |= PF_LAV_FRAME_MASTERING;
            f->mastering_max = scaled(m->max_luminance, 10000, UINT32_MAX);
            f->mastering_min = scaled(m->min_luminance, 10000, UINT32_MAX);
        }
    }
    sd = av_frame_get_side_data(src, AV_FRAME_DATA_CONTENT_LIGHT_LEVEL);
    if (sd) {
        const AVContentLightMetadata *l = (const void *)sd->data;
        f->flags |= PF_LAV_FRAME_LIGHT;
        f->max_cll = l->MaxCLL > 0xffff ? 0xffff : l->MaxCLL;
        f->max_fall = l->MaxFALL > 0xffff ? 0xffff : l->MaxFALL;
    }
}

int pf_lav_receive(pf_lav_dec *d, pf_lav_frame *f)
{
    memset(f, 0, sizeof(*f));
    int ret = avcodec_receive_frame(d->ctx, d->frame);
    if (ret == AVERROR(EAGAIN))
        return PF_LAV_AGAIN;
    if (ret == AVERROR_EOF)
        return PF_LAV_EOF;
    if (ret < 0)
        return ret;
    AVFrame *src = av_frame_alloc();
    if (!src) {
        av_frame_unref(d->frame);
        return PF_LAV_ERR_NOMEM;
    }
    av_frame_move_ref(src, d->frame);

    f->handle = src;
    for (int i = 0; i < 8; i++) {
        f->data[i] = src->extended_data ? src->extended_data[i] : src->data[i];
        f->linesize[i] = src->linesize[i];
    }
    f->pts = src->pts;
    /* libavcodec's 0 is unknown, the host's INT64_MIN. */
    f->duration = src->duration > 0 ? src->duration : INT64_MIN;
    if (src->flags & AV_FRAME_FLAG_CORRUPT)
        f->flags |= PF_LAV_FRAME_CORRUPT;
    if (src->flags & AV_FRAME_FLAG_KEY)
        f->flags |= PF_LAV_FRAME_KEY;

    if (d->ctx->codec_type == AVMEDIA_TYPE_VIDEO) {
        f->in_slot = src->buf[0] && d->slots.acquire && av_buffer_get_opaque(src->buf[0]) == d->slots.opaque;
        f->width = src->width;
        f->height = src->height;
        int range = src->color_range;
        f->pixfmt = pixfmt_of(src->format, &f->bit_depth, &f->chroma_shift_w, &f->chroma_shift_h, &range);
        f->primaries = src->color_primaries;
        f->transfer = src->color_trc;
        f->matrix = src->colorspace;
        f->range = range;
        f->chroma_location = src->chroma_location;
        side_data(src, f);
    } else {
        int channels = src->ch_layout.nb_channels;
        f->sample_rate = src->sample_rate;
        f->channels = channels;
        f->samples = src->nb_samples;
        f->sample_fmt = sample_fmt_of(src->format);
        if (src->ch_layout.order == AV_CHANNEL_ORDER_NATIVE)
            f->channel_mask = src->ch_layout.u.mask;
        /* AV_FRAME_DATA_SKIP_SAMPLES, the samples to skip from the start as a 32-bit
         * little-endian word first (libavutil/frame.h). */
        AVFrameSideData *sd = av_frame_get_side_data(src, AV_FRAME_DATA_SKIP_SAMPLES);
        if (sd && sd->size >= 4) {
            uint32_t skip = sd->data[0] | (uint32_t)sd->data[1] << 8 | (uint32_t)sd->data[2] << 16 |
                            (uint32_t)sd->data[3] << 24;
            f->skip = skip > (uint32_t)src->nb_samples ? src->nb_samples : (int32_t)skip;
        }
        /* Planes past the eighth are not handed out. */
        if ((f->sample_fmt & PF_LAV_PLANAR) && channels > 8)
            f->sample_fmt = PF_LAV_SMP_OTHER;
    }
    return PF_LAV_OK;
}

void pf_lav_frame_free(void *handle)
{
    AVFrame *f = handle;
    av_frame_free(&f);
}

void pf_lav_flush(pf_lav_dec *d)
{
    avcodec_flush_buffers(d->ctx);
    if (d->parser) {
        av_parser_close(d->parser);
        d->parser = av_parser_init(codec_id(d->codec));
    }
}

int pf_lav_parse(pf_lav_dec *d, const uint8_t *in, size_t len, int64_t pts, const uint8_t **out,
                 size_t *out_len, int64_t *out_pts)
{
    *out = NULL;
    *out_len = 0;
    *out_pts = INT64_MIN;
    if (!d->parser)
        return PF_LAV_ERR_ARG;
    int n = len > INT_MAX ? INT_MAX : (int)len;
    if (n) {
        av_fast_padded_malloc(&d->parse_buf, &d->parse_cap, (size_t)n);
        if (!d->parse_buf)
            return PF_LAV_ERR_NOMEM;
        memcpy(d->parse_buf, in, (size_t)n);
    }
    uint8_t *frame = NULL;
    int frame_len = 0;
    int used = av_parser_parse2(d->parser, d->pctx, &frame, &frame_len, n ? d->parse_buf : NULL, n,
                                pts, AV_NOPTS_VALUE, 0);
    if (used < 0)
        return used;
    if (frame_len > 0) {
        *out = frame;
        *out_len = (size_t)frame_len;
        *out_pts = d->parser->pts;
    }
    return used;
}

uint64_t pf_lav_fallbacks(const pf_lav_dec *d) { return d->fallbacks; }

void pf_lav_skip_nonref(pf_lav_dec *d, int on)
{
    d->ctx->skip_frame = on ? AVDISCARD_NONREF : AVDISCARD_DEFAULT;
}

int pf_lav_delay(const pf_lav_dec *d)
{
    /* Frame threads hold a picture back until every thread has a packet (pthread_frame.c
     * ff_thread_receive_frame). */
    int threads = d->ctx->active_thread_type & FF_THREAD_FRAME ? d->ctx->thread_count - 1 : 0;
    return d->ctx->has_b_frames + threads;
}

int pf_lav_threads(const pf_lav_dec *d)
{
    return d->ctx->active_thread_type & FF_THREAD_FRAME ? d->ctx->thread_count : 1;
}

struct pf_lav_enc {
    AVCodecContext *ctx;
    AVFrame *frame;
    AVPacket *pkt;
};

/* aacenctab.h aac_normal_chan_layouts by count, each a layout the encoder takes without
 * a PCE. */
static int enc_layout(int channels, AVChannelLayout *out)
{
    static const AVChannelLayout layouts[9] = {
        { 0 },
        AV_CHANNEL_LAYOUT_MONO,
        AV_CHANNEL_LAYOUT_STEREO,
        AV_CHANNEL_LAYOUT_SURROUND,
        AV_CHANNEL_LAYOUT_4POINT0,
        AV_CHANNEL_LAYOUT_5POINT0_BACK,
        AV_CHANNEL_LAYOUT_5POINT1_BACK,
        AV_CHANNEL_LAYOUT_6POINT1_BACK,
        AV_CHANNEL_LAYOUT_7POINT1,
    };
    if (channels < 1 || channels > 8)
        return PF_LAV_ERR_ARG;
    return av_channel_layout_copy(out, &layouts[channels]);
}

int pf_lav_enc_open(pf_lav_enc **out, int codec, int sample_rate, int channels, int64_t bit_rate,
                    int fast)
{
    *out = NULL;
    pthread_once(&log_once, quiet);
    const AVCodec *c = avcodec_find_encoder(codec_id(codec));
    if (!c || c->type != AVMEDIA_TYPE_AUDIO)
        return PF_LAV_ERR_NO_CODEC;
    if (sample_rate <= 0 || bit_rate < 0)
        return PF_LAV_ERR_ARG;
    pf_lav_enc *e = av_mallocz(sizeof(*e));
    if (!e)
        return PF_LAV_ERR_NOMEM;
    e->ctx = avcodec_alloc_context3(c);
    e->frame = av_frame_alloc();
    e->pkt = av_packet_alloc();
    if (!e->ctx || !e->frame || !e->pkt) {
        pf_lav_enc_close(e);
        return PF_LAV_ERR_NOMEM;
    }
    AVCodecContext *ctx = e->ctx;
    int ret = enc_layout(channels, &ctx->ch_layout);
    if (ret < 0) {
        pf_lav_enc_close(e);
        return ret;
    }
    ctx->thread_count = 1;
    ctx->thread_type = 0;
    ctx->sample_rate = sample_rate;
    ctx->sample_fmt = AV_SAMPLE_FMT_FLTP;
    ctx->time_base = (AVRational){ 1, sample_rate };
    ctx->bit_rate = bit_rate;
    if (fast && (ret = av_opt_set(ctx->priv_data, "aac_coder", "fast", 0)) < 0) {
        pf_lav_enc_close(e);
        return ret;
    }
    ret = avcodec_open2(ctx, c, NULL);
    if (ret < 0) {
        pf_lav_enc_close(e);
        return ret;
    }
    AVFrame *f = e->frame;
    f->format = AV_SAMPLE_FMT_FLTP;
    f->sample_rate = sample_rate;
    f->nb_samples = ctx->frame_size;
    if ((ret = av_channel_layout_copy(&f->ch_layout, &ctx->ch_layout)) < 0
        || (ret = av_frame_get_buffer(f, 0)) < 0) {
        pf_lav_enc_close(e);
        return ret;
    }
    *out = e;
    return PF_LAV_OK;
}

void pf_lav_enc_close(pf_lav_enc *e)
{
    if (!e)
        return;
    av_packet_free(&e->pkt);
    av_frame_free(&e->frame);
    avcodec_free_context(&e->ctx);
    av_free(e);
}

int pf_lav_enc_frame_size(const pf_lav_enc *e) { return e->ctx->frame_size; }

int pf_lav_enc_delay(const pf_lav_enc *e) { return e->ctx->initial_padding; }

const uint8_t *pf_lav_enc_config(const pf_lav_enc *e, size_t *len)
{
    *len = e->ctx->extradata_size > 0 ? (size_t)e->ctx->extradata_size : 0;
    return e->ctx->extradata;
}

int pf_lav_enc_send(pf_lav_enc *e, const float *pcm, int samples, int64_t pts)
{
    AVFrame *f = e->frame;
    if (!pcm || samples <= 0 || samples > e->ctx->frame_size)
        return PF_LAV_ERR_ARG;
    /* The encoder may still hold the last frame's planes. */
    int ret = av_frame_make_writable(f);
    if (ret < 0)
        return ret;
    int ch = f->ch_layout.nb_channels;
    for (int c = 0; c < ch; c++) {
        float *dst = (float *)f->extended_data[c];
        for (int i = 0; i < samples; i++)
            dst[i] = pcm[i * ch + c];
    }
    f->nb_samples = samples;
    f->pts = pts;
    ret = avcodec_send_frame(e->ctx, f);
    f->nb_samples = e->ctx->frame_size;
    if (ret == AVERROR(EAGAIN))
        return PF_LAV_AGAIN;
    if (ret == AVERROR_EOF)
        return PF_LAV_EOF;
    return ret < 0 ? ret : PF_LAV_OK;
}

int pf_lav_enc_drain(pf_lav_enc *e)
{
    int ret = avcodec_send_frame(e->ctx, NULL);
    if (ret == AVERROR_EOF)
        return PF_LAV_EOF;
    return ret < 0 ? ret : PF_LAV_OK;
}

int pf_lav_enc_receive(pf_lav_enc *e, const uint8_t **data, size_t *len, int64_t *pts, int64_t *duration)
{
    *data = NULL;
    *len = 0;
    av_packet_unref(e->pkt);
    int ret = avcodec_receive_packet(e->ctx, e->pkt);
    if (ret == AVERROR(EAGAIN))
        return PF_LAV_AGAIN;
    if (ret == AVERROR_EOF)
        return PF_LAV_EOF;
    if (ret < 0)
        return ret;
    *data = e->pkt->data;
    *len = (size_t)e->pkt->size;
    *pts = e->pkt->pts;
    *duration = e->pkt->duration;
    return PF_LAV_OK;
}

void pf_lav_strerror(int code, char *buf, size_t len)
{
    if (!len)
        return;
    if (av_strerror(code, buf, len) < 0)
        snprintf(buf, len, "error %d", code);
}

const char *pf_lav_version(void) { return FFMPEG_VERSION; }

size_t pf_lav_frame_size(void) { return sizeof(pf_lav_frame); }
