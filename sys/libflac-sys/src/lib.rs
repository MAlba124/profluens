//! Hand-written FFI to libFLAC 1.5.0. Enums are plain `c_int` constants so a value libFLAC
//! adds later is never UB. `build.rs` asserts every constant and layout against the headers.

#![allow(non_camel_case_types, non_snake_case)]

use std::os::raw::{c_int, c_void};

pub type FLAC__bool = c_int;
pub type FLAC__byte = u8;

#[repr(C)]
pub struct FLAC__StreamDecoder {
    _private: [u8; 0],
}

#[repr(C)]
pub struct FLAC__StreamEncoder {
    _private: [u8; 0],
}

pub const FLAC__MAX_CHANNELS: u32 = 8;
pub const FLAC__MAX_BLOCK_SIZE: u32 = 65535;

pub type FLAC__MetadataType = c_int;
pub const FLAC__METADATA_TYPE_STREAMINFO: FLAC__MetadataType = 0;

pub type FLAC__FrameNumberType = c_int;
pub const FLAC__FRAME_NUMBER_TYPE_FRAME_NUMBER: FLAC__FrameNumberType = 0;
pub const FLAC__FRAME_NUMBER_TYPE_SAMPLE_NUMBER: FLAC__FrameNumberType = 1;

#[repr(C)]
#[derive(Clone, Copy)]
pub union FLAC__FrameNumber {
    pub frame_number: u32,
    pub sample_number: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct FLAC__FrameHeader {
    pub blocksize: u32,
    pub sample_rate: u32,
    pub channels: u32,
    pub channel_assignment: c_int,
    pub bits_per_sample: u32,
    pub number_type: FLAC__FrameNumberType,
    pub number: FLAC__FrameNumber,
    pub crc: u8,
}

/// The head of `FLAC__Frame`, only ever read behind a pointer libFLAC passes.
#[repr(C)]
pub struct FLAC__Frame {
    pub header: FLAC__FrameHeader,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct FLAC__StreamMetadata_StreamInfo {
    pub min_blocksize: u32,
    pub max_blocksize: u32,
    pub min_framesize: u32,
    pub max_framesize: u32,
    pub sample_rate: u32,
    pub channels: u32,
    pub bits_per_sample: u32,
    pub total_samples: u64,
    pub md5sum: [u8; 16],
}

/// The head of `FLAC__StreamMetadata`. `stream_info` is read only when `type_` is STREAMINFO.
#[repr(C)]
pub struct FLAC__StreamMetadata {
    pub type_: FLAC__MetadataType,
    pub is_last: FLAC__bool,
    pub length: u32,
    pub stream_info: FLAC__StreamMetadata_StreamInfo,
}

// Mirrors the layouts build.rs asserts on the C side.
const _: () = {
    use std::mem::{offset_of, size_of};
    assert!(size_of::<FLAC__bool>() == 4);
    assert!(size_of::<FLAC__FrameHeader>() == 40);
    assert!(offset_of!(FLAC__FrameHeader, blocksize) == 0);
    assert!(offset_of!(FLAC__FrameHeader, sample_rate) == 4);
    assert!(offset_of!(FLAC__FrameHeader, channels) == 8);
    assert!(offset_of!(FLAC__FrameHeader, channel_assignment) == 12);
    assert!(offset_of!(FLAC__FrameHeader, bits_per_sample) == 16);
    assert!(offset_of!(FLAC__FrameHeader, number_type) == 20);
    assert!(offset_of!(FLAC__FrameHeader, number) == 24);
    assert!(offset_of!(FLAC__FrameHeader, crc) == 32);
    assert!(offset_of!(FLAC__Frame, header) == 0);
    assert!(size_of::<FLAC__StreamMetadata_StreamInfo>() == 56);
    assert!(offset_of!(FLAC__StreamMetadata_StreamInfo, min_blocksize) == 0);
    assert!(offset_of!(FLAC__StreamMetadata_StreamInfo, max_blocksize) == 4);
    assert!(offset_of!(FLAC__StreamMetadata_StreamInfo, min_framesize) == 8);
    assert!(offset_of!(FLAC__StreamMetadata_StreamInfo, max_framesize) == 12);
    assert!(offset_of!(FLAC__StreamMetadata_StreamInfo, sample_rate) == 16);
    assert!(offset_of!(FLAC__StreamMetadata_StreamInfo, channels) == 20);
    assert!(offset_of!(FLAC__StreamMetadata_StreamInfo, bits_per_sample) == 24);
    assert!(offset_of!(FLAC__StreamMetadata_StreamInfo, total_samples) == 32);
    assert!(offset_of!(FLAC__StreamMetadata_StreamInfo, md5sum) == 40);
    assert!(offset_of!(FLAC__StreamMetadata, type_) == 0);
    assert!(offset_of!(FLAC__StreamMetadata, is_last) == 4);
    assert!(offset_of!(FLAC__StreamMetadata, length) == 8);
    assert!(offset_of!(FLAC__StreamMetadata, stream_info) == 16);
};

pub type FLAC__StreamDecoderState = c_int;
pub const FLAC__STREAM_DECODER_SEARCH_FOR_METADATA: FLAC__StreamDecoderState = 0;
pub const FLAC__STREAM_DECODER_READ_METADATA: FLAC__StreamDecoderState = 1;
pub const FLAC__STREAM_DECODER_SEARCH_FOR_FRAME_SYNC: FLAC__StreamDecoderState = 2;
pub const FLAC__STREAM_DECODER_READ_FRAME: FLAC__StreamDecoderState = 3;
pub const FLAC__STREAM_DECODER_END_OF_STREAM: FLAC__StreamDecoderState = 4;
pub const FLAC__STREAM_DECODER_OGG_ERROR: FLAC__StreamDecoderState = 5;
pub const FLAC__STREAM_DECODER_SEEK_ERROR: FLAC__StreamDecoderState = 6;
pub const FLAC__STREAM_DECODER_ABORTED: FLAC__StreamDecoderState = 7;
pub const FLAC__STREAM_DECODER_MEMORY_ALLOCATION_ERROR: FLAC__StreamDecoderState = 8;
pub const FLAC__STREAM_DECODER_UNINITIALIZED: FLAC__StreamDecoderState = 9;
pub const FLAC__STREAM_DECODER_END_OF_LINK: FLAC__StreamDecoderState = 10;

pub type FLAC__StreamDecoderInitStatus = c_int;
pub const FLAC__STREAM_DECODER_INIT_STATUS_OK: FLAC__StreamDecoderInitStatus = 0;

pub type FLAC__StreamDecoderReadStatus = c_int;
pub const FLAC__STREAM_DECODER_READ_STATUS_CONTINUE: FLAC__StreamDecoderReadStatus = 0;
pub const FLAC__STREAM_DECODER_READ_STATUS_END_OF_STREAM: FLAC__StreamDecoderReadStatus = 1;
pub const FLAC__STREAM_DECODER_READ_STATUS_ABORT: FLAC__StreamDecoderReadStatus = 2;

pub type FLAC__StreamDecoderSeekStatus = c_int;
pub const FLAC__STREAM_DECODER_SEEK_STATUS_OK: FLAC__StreamDecoderSeekStatus = 0;
pub const FLAC__STREAM_DECODER_SEEK_STATUS_ERROR: FLAC__StreamDecoderSeekStatus = 1;

pub type FLAC__StreamDecoderLengthStatus = c_int;
pub const FLAC__STREAM_DECODER_LENGTH_STATUS_UNSUPPORTED: FLAC__StreamDecoderLengthStatus = 2;

pub type FLAC__StreamDecoderTellStatus = c_int;
pub const FLAC__STREAM_DECODER_TELL_STATUS_OK: FLAC__StreamDecoderTellStatus = 0;
pub const FLAC__STREAM_DECODER_TELL_STATUS_ERROR: FLAC__StreamDecoderTellStatus = 1;

pub type FLAC__StreamDecoderWriteStatus = c_int;
pub const FLAC__STREAM_DECODER_WRITE_STATUS_CONTINUE: FLAC__StreamDecoderWriteStatus = 0;
pub const FLAC__STREAM_DECODER_WRITE_STATUS_ABORT: FLAC__StreamDecoderWriteStatus = 1;

pub type FLAC__StreamDecoderErrorStatus = c_int;
pub const FLAC__STREAM_DECODER_ERROR_STATUS_LOST_SYNC: FLAC__StreamDecoderErrorStatus = 0;
pub const FLAC__STREAM_DECODER_ERROR_STATUS_BAD_HEADER: FLAC__StreamDecoderErrorStatus = 1;
pub const FLAC__STREAM_DECODER_ERROR_STATUS_FRAME_CRC_MISMATCH: FLAC__StreamDecoderErrorStatus = 2;
pub const FLAC__STREAM_DECODER_ERROR_STATUS_UNPARSEABLE_STREAM: FLAC__StreamDecoderErrorStatus = 3;
pub const FLAC__STREAM_DECODER_ERROR_STATUS_BAD_METADATA: FLAC__StreamDecoderErrorStatus = 4;
pub const FLAC__STREAM_DECODER_ERROR_STATUS_OUT_OF_BOUNDS: FLAC__StreamDecoderErrorStatus = 5;
pub const FLAC__STREAM_DECODER_ERROR_STATUS_MISSING_FRAME: FLAC__StreamDecoderErrorStatus = 6;

pub type FLAC__StreamDecoderReadCallback = Option<
    unsafe extern "C" fn(
        decoder: *const FLAC__StreamDecoder,
        buffer: *mut FLAC__byte,
        bytes: *mut usize,
        client_data: *mut c_void,
    ) -> FLAC__StreamDecoderReadStatus,
>;
pub type FLAC__StreamDecoderSeekCallback = Option<
    unsafe extern "C" fn(
        decoder: *const FLAC__StreamDecoder,
        absolute_byte_offset: u64,
        client_data: *mut c_void,
    ) -> FLAC__StreamDecoderSeekStatus,
>;
pub type FLAC__StreamDecoderTellCallback = Option<
    unsafe extern "C" fn(
        decoder: *const FLAC__StreamDecoder,
        absolute_byte_offset: *mut u64,
        client_data: *mut c_void,
    ) -> FLAC__StreamDecoderTellStatus,
>;
pub type FLAC__StreamDecoderLengthCallback = Option<
    unsafe extern "C" fn(
        decoder: *const FLAC__StreamDecoder,
        stream_length: *mut u64,
        client_data: *mut c_void,
    ) -> FLAC__StreamDecoderLengthStatus,
>;
pub type FLAC__StreamDecoderEofCallback = Option<
    unsafe extern "C" fn(
        decoder: *const FLAC__StreamDecoder,
        client_data: *mut c_void,
    ) -> FLAC__bool,
>;
pub type FLAC__StreamDecoderWriteCallback = Option<
    unsafe extern "C" fn(
        decoder: *const FLAC__StreamDecoder,
        frame: *const FLAC__Frame,
        buffer: *const *const i32,
        client_data: *mut c_void,
    ) -> FLAC__StreamDecoderWriteStatus,
>;
pub type FLAC__StreamDecoderMetadataCallback = Option<
    unsafe extern "C" fn(
        decoder: *const FLAC__StreamDecoder,
        metadata: *const FLAC__StreamMetadata,
        client_data: *mut c_void,
    ),
>;
pub type FLAC__StreamDecoderErrorCallback = Option<
    unsafe extern "C" fn(
        decoder: *const FLAC__StreamDecoder,
        status: FLAC__StreamDecoderErrorStatus,
        client_data: *mut c_void,
    ),
>;

unsafe extern "C" {
    pub fn FLAC__stream_decoder_new() -> *mut FLAC__StreamDecoder;
    pub fn FLAC__stream_decoder_delete(decoder: *mut FLAC__StreamDecoder);
    pub fn FLAC__stream_decoder_init_stream(
        decoder: *mut FLAC__StreamDecoder,
        read_callback: FLAC__StreamDecoderReadCallback,
        seek_callback: FLAC__StreamDecoderSeekCallback,
        tell_callback: FLAC__StreamDecoderTellCallback,
        length_callback: FLAC__StreamDecoderLengthCallback,
        eof_callback: FLAC__StreamDecoderEofCallback,
        write_callback: FLAC__StreamDecoderWriteCallback,
        metadata_callback: FLAC__StreamDecoderMetadataCallback,
        error_callback: FLAC__StreamDecoderErrorCallback,
        client_data: *mut c_void,
    ) -> FLAC__StreamDecoderInitStatus;
    pub fn FLAC__stream_decoder_get_state(
        decoder: *const FLAC__StreamDecoder,
    ) -> FLAC__StreamDecoderState;
    pub fn FLAC__stream_decoder_get_decode_position(
        decoder: *const FLAC__StreamDecoder,
        position: *mut u64,
    ) -> FLAC__bool;
    pub fn FLAC__stream_decoder_flush(decoder: *mut FLAC__StreamDecoder) -> FLAC__bool;
    pub fn FLAC__stream_decoder_process_single(decoder: *mut FLAC__StreamDecoder) -> FLAC__bool;
}

pub type FLAC__StreamEncoderState = c_int;
pub const FLAC__STREAM_ENCODER_OK: FLAC__StreamEncoderState = 0;
pub const FLAC__STREAM_ENCODER_UNINITIALIZED: FLAC__StreamEncoderState = 1;
pub const FLAC__STREAM_ENCODER_MEMORY_ALLOCATION_ERROR: FLAC__StreamEncoderState = 8;

pub type FLAC__StreamEncoderInitStatus = c_int;
pub const FLAC__STREAM_ENCODER_INIT_STATUS_OK: FLAC__StreamEncoderInitStatus = 0;
pub const FLAC__STREAM_ENCODER_INIT_STATUS_NOT_STREAMABLE: FLAC__StreamEncoderInitStatus = 11;

pub type FLAC__StreamEncoderWriteStatus = c_int;
pub const FLAC__STREAM_ENCODER_WRITE_STATUS_OK: FLAC__StreamEncoderWriteStatus = 0;
pub const FLAC__STREAM_ENCODER_WRITE_STATUS_FATAL_ERROR: FLAC__StreamEncoderWriteStatus = 1;

pub type FLAC__StreamEncoderWriteCallback = Option<
    unsafe extern "C" fn(
        encoder: *const FLAC__StreamEncoder,
        buffer: *const FLAC__byte,
        bytes: usize,
        samples: u32,
        current_frame: u32,
        client_data: *mut c_void,
    ) -> FLAC__StreamEncoderWriteStatus,
>;
/// Untyped, the output is never seekable from libFLAC's side.
pub type FLAC__StreamEncoderSeekCallback = Option<unsafe extern "C" fn()>;
pub type FLAC__StreamEncoderTellCallback = Option<unsafe extern "C" fn()>;
pub type FLAC__StreamEncoderMetadataCallback = Option<
    unsafe extern "C" fn(
        encoder: *const FLAC__StreamEncoder,
        metadata: *const FLAC__StreamMetadata,
        client_data: *mut c_void,
    ),
>;

#[cfg(feature = "encoder")]
unsafe extern "C" {
    pub fn FLAC__stream_encoder_new() -> *mut FLAC__StreamEncoder;
    pub fn FLAC__stream_encoder_delete(encoder: *mut FLAC__StreamEncoder);
    pub fn FLAC__stream_encoder_set_streamable_subset(
        encoder: *mut FLAC__StreamEncoder,
        value: FLAC__bool,
    ) -> FLAC__bool;
    pub fn FLAC__stream_encoder_set_channels(
        encoder: *mut FLAC__StreamEncoder,
        value: u32,
    ) -> FLAC__bool;
    pub fn FLAC__stream_encoder_set_bits_per_sample(
        encoder: *mut FLAC__StreamEncoder,
        value: u32,
    ) -> FLAC__bool;
    pub fn FLAC__stream_encoder_set_sample_rate(
        encoder: *mut FLAC__StreamEncoder,
        value: u32,
    ) -> FLAC__bool;
    pub fn FLAC__stream_encoder_set_compression_level(
        encoder: *mut FLAC__StreamEncoder,
        value: u32,
    ) -> FLAC__bool;
    pub fn FLAC__stream_encoder_set_blocksize(
        encoder: *mut FLAC__StreamEncoder,
        value: u32,
    ) -> FLAC__bool;
    pub fn FLAC__stream_encoder_get_state(
        encoder: *const FLAC__StreamEncoder,
    ) -> FLAC__StreamEncoderState;
    pub fn FLAC__stream_encoder_init_stream(
        encoder: *mut FLAC__StreamEncoder,
        write_callback: FLAC__StreamEncoderWriteCallback,
        seek_callback: FLAC__StreamEncoderSeekCallback,
        tell_callback: FLAC__StreamEncoderTellCallback,
        metadata_callback: FLAC__StreamEncoderMetadataCallback,
        client_data: *mut c_void,
    ) -> FLAC__StreamEncoderInitStatus;
    pub fn FLAC__stream_encoder_finish(encoder: *mut FLAC__StreamEncoder) -> FLAC__bool;
    pub fn FLAC__stream_encoder_process_interleaved(
        encoder: *mut FLAC__StreamEncoder,
        buffer: *const i32,
        samples: u32,
    ) -> FLAC__bool;
}
