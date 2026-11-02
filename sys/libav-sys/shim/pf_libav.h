/* The flat, pf-owned interface over libavcodec. Rust binds this header only, never
 * an FFmpeg struct. Every type here is laid out by pf and checked on both sides. */
#ifndef PF_LIBAV_H
#define PF_LIBAV_H

#include <stddef.h>
#include <stdint.h>

/* Return codes. Negative values past these are AVERROR codes passed through. */
#define PF_LAV_OK 0
#define PF_LAV_AGAIN 1
#define PF_LAV_EOF 2
#define PF_LAV_ERR_NO_CODEC (-0x50460001)
#define PF_LAV_ERR_NOMEM (-0x50460002)
#define PF_LAV_ERR_ARG (-0x50460003)

/* The codecs this build carries. */
enum {
    PF_LAV_H264 = 1,
    PF_LAV_HEVC = 2,
    PF_LAV_VP8 = 3,
    PF_LAV_VP9 = 4,
    PF_LAV_AAC = 5,
    PF_LAV_AC3 = 6,
    PF_LAV_EAC3 = 7,
    PF_LAV_MP3 = 8,
    PF_LAV_DTS = 9,
    PF_LAV_TRUEHD = 10,
    PF_LAV_MLP = 11,
    PF_LAV_MPEG2 = 12,
    PF_LAV_VORBIS = 13,
    PF_LAV_MPEG4 = 14,
    PF_LAV_ALAC = 15,
    PF_LAV_FLV1 = 16,
    PF_LAV_VP6 = 17,
    PF_LAV_VP6F = 18,
    PF_LAV_VP6A = 19,
    PF_LAV_VP7 = 20,
};

/* Picture layouts the vocabulary names. YUV is three planes of any depth and
 * subsampling, words little-endian past 8 bits. YUV and OTHER carry their depth and
 * subsampling. */
enum {
    PF_LAV_PIX_NONE = 0,
    PF_LAV_PIX_I420 = 1,
    PF_LAV_PIX_GRAY8 = 2,
    PF_LAV_PIX_I420_10 = 3,
    PF_LAV_PIX_YUV = 4,
    PF_LAV_PIX_OTHER = 255,
};

/* Sample formats, PF_LAV_PLANAR or'd in for planar. */
enum {
    PF_LAV_SMP_NONE = 0,
    PF_LAV_SMP_U8 = 1,
    PF_LAV_SMP_S16 = 2,
    PF_LAV_SMP_S32 = 3,
    PF_LAV_SMP_FLT = 4,
    PF_LAV_SMP_DBL = 5,
    PF_LAV_SMP_OTHER = 15,
    PF_LAV_PLANAR = 16,
};

/* pf_lav_frame.flags */
#define PF_LAV_FRAME_CORRUPT 1
#define PF_LAV_FRAME_KEY 2
#define PF_LAV_FRAME_MASTERING 4
#define PF_LAV_FRAME_LIGHT 8

/* Where video pictures are decoded. acquire returns the base of a host slot of at
 * least `bytes`, or NULL to have libavcodec use its own memory. release gives back
 * the slot a pointer lies in, once the last reference to it is gone, which may be a
 * frame dropped after the decoder closed, so opaque must outlive both. acquire is
 * called on the thread driving the decoder, release wherever a frame is dropped. */
typedef struct pf_lav_slots {
    void *opaque;
    uint8_t *(*acquire)(void *opaque, size_t bytes);
    void (*release)(void *opaque, uint8_t *ptr);
} pf_lav_slots;

/* One decoded frame. `handle` owns it until pf_lav_frame_free. */
typedef struct pf_lav_frame {
    void *handle;
    uint8_t *data[8];
    int64_t linesize[8];
    /* Non-zero when the planes lie in a slot from pf_lav_slots.acquire. */
    int64_t in_slot;
    int64_t pts;
    int64_t duration;
    int32_t width;
    int32_t height;
    int32_t pixfmt;
    int32_t bit_depth;
    int32_t chroma_shift_w;
    int32_t chroma_shift_h;
    /* ITU-T H.273 code points, range 0 unspecified, 1 limited, 2 full. */
    int32_t primaries;
    int32_t transfer;
    int32_t matrix;
    int32_t range;
    int32_t flags;
    /* SMPTE ST 2086 in 0.00002 units (chromaticity) and 0.0001 cd/m2 (luminance). */
    uint16_t mastering_primaries[6];
    uint16_t mastering_white[2];
    uint32_t mastering_max;
    uint32_t mastering_min;
    uint16_t max_cll;
    uint16_t max_fall;
    int32_t sample_rate;
    int32_t channels;
    int32_t samples;
    int32_t sample_fmt;
    /* The channels in native order as a WAVE style mask, 0 when not native. */
    uint64_t channel_mask;
    /* AVChromaLocation, 0 unspecified, else chroma_sample_loc_type + 1. */
    int32_t chroma_location;
    /* Samples at the front of an audio frame libavcodec would have dropped, the stream's
     * start or what a flush leaves to converge. Only Vorbis exports it, its frames come
     * out whole. */
    int32_t skip;
} pf_lav_frame;

typedef struct pf_lav_dec pf_lav_dec;

/* Open a single threaded decoder. `slots` may be NULL. `parse` adds a parser
 * that frames a byte stream (pf_lav_parse). Vorbis takes its three headers as
 * extradata, Xiph laced or each behind a 16-bit big-endian length. */
int pf_lav_open(pf_lav_dec **out, int codec, const uint8_t *extradata, size_t extradata_len,
                const pf_lav_slots *slots, int parse);
void pf_lav_close(pf_lav_dec *d);

/* One packet, copied into padded memory. pts, dts and duration are carried to the
 * frame as given, INT64_MIN for none. */
int pf_lav_send(pf_lav_dec *d, const uint8_t *data, size_t len, int64_t pts, int64_t dts,
                int64_t duration);
/* End of stream. Receive until PF_LAV_EOF, then pf_lav_flush to decode again. */
int pf_lav_drain(pf_lav_dec *d);
int pf_lav_receive(pf_lav_dec *d, pf_lav_frame *f);
void pf_lav_frame_free(void *handle);
/* Drop all frames and stream state, and the parser's partial frame. */
void pf_lav_flush(pf_lav_dec *d);

/* Feed bytes to the parser. Returns the bytes consumed, or a negative code. A whole
 * frame, when one completed, is put in *out, valid until the next call. An empty
 * input flushes the last frame at end of stream. */
int pf_lav_parse(pf_lav_dec *d, const uint8_t *in, size_t len, int64_t pts, const uint8_t **out,
                 size_t *out_len, int64_t *out_pts);

/* Skip non-reference pictures from the next packet on, or decode all again. */
void pf_lav_skip_nonref(pf_lav_dec *d, int on);

/* Pictures the decoder holds back for reordering, has_b_frames, the decoder being single
 * threaded. */
int pf_lav_delay(const pf_lav_dec *d);

/* Pictures libavcodec had to place in its own memory, since open. */
uint64_t pf_lav_fallbacks(const pf_lav_dec *d);

/* An audio encoder, single threaded, of interleaved float in frames of
 * pf_lav_enc_frame_size samples. */
typedef struct pf_lav_enc pf_lav_enc;

/* Open an encoder, PF_LAV_AAC for AAC-LC. channels 1 to 8 in the codec's order for that
 * count (mono, stereo, 3.0, 4.0, 5.0, 5.1, 6.1, 7.1, the surrounds at the back),
 * bit_rate 0 for the codec's own choice. `fast` picks the cheaper search, the AAC
 * encoder's `aac_coder=fast` in place of `twoloop`. */
int pf_lav_enc_open(pf_lav_enc **out, int codec, int sample_rate, int channels, int64_t bit_rate,
                    int fast);
void pf_lav_enc_close(pf_lav_enc *e);
/* Samples a frame holds, and the priming samples ahead of the first. */
int pf_lav_enc_frame_size(const pf_lav_enc *e);
int pf_lav_enc_delay(const pf_lav_enc *e);
/* The codec configuration, an AudioSpecificConfig for AAC, valid until close. */
const uint8_t *pf_lav_enc_config(const pf_lav_enc *e, size_t *len);
/* One frame, frame_size samples of all channels, fewer only for the last. pts in
 * samples. */
int pf_lav_enc_send(pf_lav_enc *e, const float *pcm, int samples, int64_t pts);
/* End of stream. Receive until PF_LAV_EOF, the encoder is done then. */
int pf_lav_enc_drain(pf_lav_enc *e);
/* The next packet, valid until the next receive or close. pts and duration in samples,
 * a packet's pts its first sample after decoding, the priming taken off. */
int pf_lav_enc_receive(pf_lav_enc *e, const uint8_t **data, size_t *len, int64_t *pts, int64_t *duration);

void pf_lav_strerror(int code, char *buf, size_t len);
const char *pf_lav_version(void);
size_t pf_lav_frame_size(void);

#endif
