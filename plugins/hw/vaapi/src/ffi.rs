//! Hand-rolled FFI to libva and libva-drm, transcribed from the libva 2.23/2.24 headers.
//! Bitfield unions are their `uint32_t value` arm, packed by hand. Layout tests lock the sizes.

#![allow(non_snake_case, non_camel_case_types, non_upper_case_globals, dead_code)]

use std::ffi::{c_char, c_int, c_void};

pub type VADisplay = *mut c_void;
pub type VAStatus = c_int;
pub type VAGenericID = u32;
pub type VAConfigID = VAGenericID;
pub type VAContextID = VAGenericID;
pub type VASurfaceID = VAGenericID;
pub type VABufferID = VAGenericID;
pub type VAImageID = VAGenericID;
pub type VAProfile = c_int;
pub type VAEntrypoint = c_int;
pub type VAConfigAttribType = c_int;
pub type VABufferType = c_int;
pub type VAGenericValueType = c_int;
pub type VASurfaceAttribType = c_int;

pub const VA_STATUS_SUCCESS: VAStatus = 0;
/// The statuses that say the device is not done yet (va.h:280-307).
pub const VA_STATUS_ERROR_SURFACE_BUSY: VAStatus = 0x10;
pub const VA_STATUS_ERROR_SURFACE_IN_DISPLAYING: VAStatus = 0x15;
pub const VA_STATUS_ERROR_HW_BUSY: VAStatus = 0x22;
/// `VACodedBufferSegment.status` bits of a picture to discard: SLICE_OVERFLOW,
/// FRAME_SIZE_OVERFLOW and BAD_BITSTREAM.
pub const VA_CODED_BUF_STATUS_BROKEN: u32 = 0x200 | 0x1000 | 0x8000;
pub const VA_STATUS_ERROR_TIMEDOUT: VAStatus = 0x26;
pub const VA_STATUS_ERROR_UNIMPLEMENTED: VAStatus = 0x14;
pub const VA_INVALID_ID: VAGenericID = 0xffff_ffff;
pub const VA_INVALID_SURFACE: VASurfaceID = VA_INVALID_ID;

pub const VAProfileNone: VAProfile = -1;
pub const VAProfileH264Main: VAProfile = 6;
pub const VAProfileH264High: VAProfile = 7;
pub const VAProfileH264ConstrainedBaseline: VAProfile = 13;
pub const VAProfileVP8Version0_3: VAProfile = 14;
pub const VAProfileHEVCMain: VAProfile = 17;
pub const VAProfileHEVCMain10: VAProfile = 18;
pub const VAProfileHEVCMain12: VAProfile = 23;
pub const VAProfileVP9Profile0: VAProfile = 19;
pub const VAProfileVP9Profile2: VAProfile = 21;
pub const VAProfileAV1Profile0: VAProfile = 32;

pub const VAEntrypointVLD: VAEntrypoint = 1;
pub const VAEntrypointEncSlice: VAEntrypoint = 6;
pub const VAEntrypointEncSliceLP: VAEntrypoint = 8;
pub const VAEntrypointVideoProc: VAEntrypoint = 10;

pub const VAConfigAttribRTFormat: VAConfigAttribType = 0;
pub const VAConfigAttribDecProcessing: VAConfigAttribType = 8;
pub const VAConfigAttribPredictionDirection: VAConfigAttribType = 39;
pub const VA_PREDICTION_DIRECTION_BI_NOT_EMPTY: u32 = 0x0000_0004;
pub const VA_DEC_PROCESSING: u32 = 0x0000_0001;
pub const VA_CHROMA_SITING_VERTICAL_CENTER: u8 = 0x02;
pub const VA_CHROMA_SITING_HORIZONTAL_LEFT: u8 = 0x04;
pub const VAConfigAttribRateControl: VAConfigAttribType = 5;
pub const VAConfigAttribEncPackedHeaders: VAConfigAttribType = 10;
pub const VAConfigAttribEncMaxRefFrames: VAConfigAttribType = 13;
pub const VA_RT_FORMAT_YUV420: u32 = 0x0000_0001;
pub const VA_RT_FORMAT_YUV420_10: u32 = 0x0000_0100;
pub const VA_RT_FORMAT_YUV422: u32 = 0x0000_0002;
pub const VA_RT_FORMAT_YUV444: u32 = 0x0000_0004;
pub const VA_RT_FORMAT_YUV422_10: u32 = 0x0000_0200;
pub const VA_RT_FORMAT_YUV444_10: u32 = 0x0000_0400;
pub const VA_RT_FORMAT_YUV420_12: u32 = 0x0000_1000;
pub const VA_RT_FORMAT_YUV422_12: u32 = 0x0000_2000;
pub const VA_RT_FORMAT_YUV444_12: u32 = 0x0000_4000;
pub const VA_ATTRIB_NOT_SUPPORTED: u32 = 0x8000_0000;

pub const VA_RC_CBR: u32 = 0x0000_0002;
pub const VA_RC_VBR: u32 = 0x0000_0004;
pub const VA_RC_CQP: u32 = 0x0000_0010;

pub const VA_ENC_PACKED_HEADER_NONE: u32 = 0x0000_0000;
pub const VA_ENC_PACKED_HEADER_SEQUENCE: u32 = 0x0000_0001;
pub const VA_ENC_PACKED_HEADER_PICTURE: u32 = 0x0000_0002;
pub const VA_ENC_PACKED_HEADER_SLICE: u32 = 0x0000_0004;
pub const VA_ENC_PACKED_HEADER_MISC: u32 = 0x0000_0008;
pub const VA_ENC_PACKED_HEADER_RAW_DATA: u32 = 0x0000_0010;

pub const VAPictureParameterBufferType: VABufferType = 0;
pub const VAIQMatrixBufferType: VABufferType = 1;
pub const VASliceParameterBufferType: VABufferType = 4;
pub const VASliceDataBufferType: VABufferType = 5;
pub const VAQMatrixBufferType: VABufferType = 11;
pub const VAEncCodedBufferType: VABufferType = 21;
pub const VAEncSequenceParameterBufferType: VABufferType = 22;
pub const VAEncPictureParameterBufferType: VABufferType = 23;
pub const VAEncSliceParameterBufferType: VABufferType = 24;
pub const VAEncPackedHeaderParameterBufferType: VABufferType = 25;
pub const VAEncPackedHeaderDataBufferType: VABufferType = 26;
pub const VAEncMiscParameterBufferType: VABufferType = 27;
pub const VAProcPipelineParameterBufferType: VABufferType = 41;
pub const VAProcFilterParameterBufferType: VABufferType = 42;

pub const VAEncPackedHeaderSequence: u32 = 1;
pub const VAEncPackedHeaderPicture: u32 = 2;
pub const VAEncPackedHeaderSlice: u32 = 3;
pub const VAEncPackedHeaderRawData: u32 = 4;

pub const VAEncMiscParameterTypeFrameRate: u32 = 0;
pub const VAEncMiscParameterTypeRateControl: u32 = 1;
pub const VAEncMiscParameterTypeHRD: u32 = 5;
pub const VAEncMiscParameterTypeQualityLevel: u32 = 6;

pub const VAGenericValueTypeInteger: VAGenericValueType = 1;
pub const VAGenericValueTypePointer: VAGenericValueType = 3;

pub const VASurfaceAttribPixelFormat: VASurfaceAttribType = 1;
pub const VASurfaceAttribMemoryType: VASurfaceAttribType = 6;
pub const VASurfaceAttribExternalBufferDescriptor: VASurfaceAttribType = 7;
pub const VA_SURFACE_ATTRIB_SETTABLE: u32 = 0x0000_0002;
pub const VA_SURFACE_ATTRIB_MEM_TYPE_VA: u32 = 0x0000_0001;

/// PRIME_2 carries per-layer DRM modifiers, the legacy PRIME (0x20000000) does not.
pub const VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2: u32 = 0x4000_0000;

pub const VA_EXPORT_SURFACE_READ_ONLY: u32 = 0x0001;
pub const VA_EXPORT_SURFACE_WRITE_ONLY: u32 = 0x0002;
pub const VA_EXPORT_SURFACE_SEPARATE_LAYERS: u32 = 0x0004;
/// One layer holding every plane, what an EGL NV12 import wants.
pub const VA_EXPORT_SURFACE_COMPOSED_LAYERS: u32 = 0x0008;

pub const VA_DRM_PRIME_MAX_OBJECTS: usize = 4;
pub const VA_DRM_PRIME_MAX_LAYERS: usize = 4;
pub const VA_DRM_PRIME_MAX_PLANES: usize = 4;

/// `fd` is owned by the caller after export and must be closed.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VADRMPRIMESurfaceDescriptorObject {
    pub fd: c_int,
    pub size: u32,
    pub drm_format_modifier: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VADRMPRIMESurfaceDescriptorLayer {
    pub drm_format: u32,
    pub num_planes: u32,
    pub object_index: [u32; VA_DRM_PRIME_MAX_PLANES],
    pub offset: [u32; VA_DRM_PRIME_MAX_PLANES],
    pub pitch: [u32; VA_DRM_PRIME_MAX_PLANES],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VADRMPRIMESurfaceDescriptor {
    /// The surface FourCC, not the DRM one, which is per-layer.
    pub fourcc: u32,
    pub width: u32,
    pub height: u32,
    pub num_objects: u32,
    pub objects: [VADRMPRIMESurfaceDescriptorObject; VA_DRM_PRIME_MAX_OBJECTS],
    pub num_layers: u32,
    pub layers: [VADRMPRIMESurfaceDescriptorLayer; VA_DRM_PRIME_MAX_LAYERS],
}

pub const VA_FOURCC_NV12: u32 = 0x3231_564E;
pub use profluens_video::gpuframe::DRM_FORMAT_NV12;
pub const VA_FOURCC_P010: u32 = 0x3031_3050;
pub const VA_FOURCC_P012: u32 = 0x3231_3050;
/// With this modifier the EGL import must omit the modifier attributes, some drivers reject
/// INVALID.
pub use profluens_video::gpuframe::DRM_FORMAT_MOD_INVALID;
pub use profluens_video::gpuframe::DRM_FORMAT_P010;
pub const VA_PROGRESSIVE: c_int = 0x1;
pub const VA_SLICE_DATA_FLAG_ALL: u32 = 0x00;

pub const VA_PICTURE_H264_INVALID: u32 = 0x0000_0001;
pub const VA_PICTURE_H264_TOP_FIELD: u32 = 0x0000_0002;
pub const VA_PICTURE_H264_BOTTOM_FIELD: u32 = 0x0000_0004;
pub const VA_PICTURE_H264_SHORT_TERM_REFERENCE: u32 = 0x0000_0008;
pub const VA_PICTURE_H264_LONG_TERM_REFERENCE: u32 = 0x0000_0010;

pub const VA_PADDING_LOW: usize = 4;
pub const VA_PADDING_MEDIUM: usize = 8;
pub const VA_PADDING_HIGH: usize = 16;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAConfigAttrib {
    pub type_: VAConfigAttribType,
    pub value: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union VAGenericValueUnion {
    pub i: i32,
    pub f: f32,
    pub p: *mut c_void,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAGenericValue {
    pub type_: VAGenericValueType,
    pub value: VAGenericValueUnion,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VASurfaceAttrib {
    pub type_: VASurfaceAttribType,
    pub flags: u32,
    pub value: VAGenericValue,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAImageFormat {
    pub fourcc: u32,
    pub byte_order: u32,
    pub bits_per_pixel: u32,
    pub depth: u32,
    pub red_mask: u32,
    pub green_mask: u32,
    pub blue_mask: u32,
    pub alpha_mask: u32,
    pub va_reserved: [u32; VA_PADDING_LOW],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAImage {
    pub image_id: VAImageID,
    pub format: VAImageFormat,
    pub buf: VABufferID,
    pub width: u16,
    pub height: u16,
    pub data_size: u32,
    pub num_planes: u32,
    pub pitches: [u32; 3],
    pub offsets: [u32; 3],
    pub num_palette_entries: i32,
    pub entry_bytes: i32,
    pub component_order: [i8; 4],
    pub va_reserved: [u32; VA_PADDING_LOW],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAPictureH264 {
    pub picture_id: VASurfaceID,
    pub frame_idx: u32,
    pub flags: u32,
    pub TopFieldOrderCnt: i32,
    pub BottomFieldOrderCnt: i32,
    pub va_reserved: [u32; VA_PADDING_LOW],
}

impl VAPictureH264 {
    /// Every unused slot must be this, the driver stops at the first INVALID entry (H.264 8.2.4.2).
    pub fn invalid() -> Self {
        VAPictureH264 {
            picture_id: VA_INVALID_SURFACE,
            frame_idx: 0,
            flags: VA_PICTURE_H264_INVALID,
            TopFieldOrderCnt: 0,
            BottomFieldOrderCnt: 0,
            va_reserved: [0; VA_PADDING_LOW],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAPictureParameterBufferH264 {
    pub CurrPic: VAPictureH264,
    pub ReferenceFrames: [VAPictureH264; 16],
    pub picture_width_in_mbs_minus1: u16,
    pub picture_height_in_mbs_minus1: u16,
    pub bit_depth_luma_minus8: u8,
    pub bit_depth_chroma_minus8: u8,
    pub num_ref_frames: u8,
    pub seq_fields: u32,
    pub num_slice_groups_minus1: u8,
    pub slice_group_map_type: u8,
    pub slice_group_change_rate_minus1: u16,
    pub pic_init_qp_minus26: i8,
    pub pic_init_qs_minus26: i8,
    pub chroma_qp_index_offset: i8,
    pub second_chroma_qp_index_offset: i8,
    pub pic_fields: u32,
    pub frame_num: u16,
    pub va_reserved: [u32; VA_PADDING_MEDIUM],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAIQMatrixBufferH264 {
    pub ScalingList4x4: [[u8; 16]; 6],
    pub ScalingList8x8: [[u8; 64]; 2],
    pub va_reserved: [u32; VA_PADDING_LOW],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VASliceParameterBufferH264 {
    pub slice_data_size: u32,
    pub slice_data_offset: u32,
    pub slice_data_flag: u32,
    pub slice_data_bit_offset: u16,
    pub first_mb_in_slice: u16,
    pub slice_type: u8,
    pub direct_spatial_mv_pred_flag: u8,
    pub num_ref_idx_l0_active_minus1: u8,
    pub num_ref_idx_l1_active_minus1: u8,
    pub cabac_init_idc: u8,
    pub slice_qp_delta: i8,
    pub disable_deblocking_filter_idc: u8,
    pub slice_alpha_c0_offset_div2: i8,
    pub slice_beta_offset_div2: i8,
    pub RefPicList0: [VAPictureH264; 32],
    pub RefPicList1: [VAPictureH264; 32],
    pub luma_log2_weight_denom: u8,
    pub chroma_log2_weight_denom: u8,
    pub luma_weight_l0_flag: u8,
    pub luma_weight_l0: [i16; 32],
    pub luma_offset_l0: [i16; 32],
    pub chroma_weight_l0_flag: u8,
    pub chroma_weight_l0: [[i16; 2]; 32],
    pub chroma_offset_l0: [[i16; 2]; 32],
    pub luma_weight_l1_flag: u8,
    pub luma_weight_l1: [i16; 32],
    pub luma_offset_l1: [i16; 32],
    pub chroma_weight_l1_flag: u8,
    pub chroma_weight_l1: [[i16; 2]; 32],
    pub chroma_offset_l1: [[i16; 2]; 32],
    pub va_reserved: [u32; VA_PADDING_LOW],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VACodedBufferSegment {
    pub size: u32,
    pub bit_offset: u32,
    pub status: u32,
    pub reserved: u32,
    pub buf: *mut c_void,
    pub next: *mut c_void,
    pub va_reserved: [u32; VA_PADDING_LOW],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAEncPackedHeaderParameterBuffer {
    pub type_: u32,
    pub bit_length: u32,
    pub has_emulation_bytes: u8,
    pub va_reserved: [u32; VA_PADDING_LOW],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAEncMiscParameterRateControl {
    pub bits_per_second: u32,
    pub target_percentage: u32,
    pub window_size: u32,
    pub initial_qp: u32,
    pub min_qp: u32,
    pub basic_unit_size: u32,
    pub rc_flags: u32,
    pub ICQ_quality_factor: u32,
    pub max_qp: u32,
    pub quality_factor: u32,
    pub target_frame_size: u32,
    pub va_reserved: [u32; VA_PADDING_LOW],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAEncMiscParameterFrameRate {
    pub framerate: u32,
    pub framerate_flags: u32,
    pub va_reserved: [u32; VA_PADDING_LOW],
}

/// 1 best to 7 fastest, 0 the driver's default (iHD's is 7).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAEncMiscParameterBufferQualityLevel {
    pub quality_level: u32,
    pub va_reserved: [u32; VA_PADDING_LOW],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAEncMiscParameterHRD {
    pub initial_buffer_fullness: u32,
    pub buffer_size: u32,
    pub va_reserved: [u32; VA_PADDING_LOW],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAEncSequenceParameterBufferH264 {
    pub seq_parameter_set_id: u8,
    pub level_idc: u8,
    pub intra_period: u32,
    pub intra_idr_period: u32,
    pub ip_period: u32,
    pub bits_per_second: u32,
    pub max_num_ref_frames: u32,
    pub picture_width_in_mbs: u16,
    pub picture_height_in_mbs: u16,
    pub seq_fields: u32,
    pub bit_depth_luma_minus8: u8,
    pub bit_depth_chroma_minus8: u8,
    pub num_ref_frames_in_pic_order_cnt_cycle: u8,
    pub offset_for_non_ref_pic: i32,
    pub offset_for_top_to_bottom_field: i32,
    pub offset_for_ref_frame: [i32; 256],
    pub frame_cropping_flag: u8,
    pub frame_crop_left_offset: u32,
    pub frame_crop_right_offset: u32,
    pub frame_crop_top_offset: u32,
    pub frame_crop_bottom_offset: u32,
    pub vui_parameters_present_flag: u8,
    pub vui_fields: u32,
    pub aspect_ratio_idc: u8,
    pub sar_width: u32,
    pub sar_height: u32,
    pub num_units_in_tick: u32,
    pub time_scale: u32,
    pub va_reserved: [u32; VA_PADDING_LOW],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAEncPictureParameterBufferH264 {
    pub CurrPic: VAPictureH264,
    pub ReferenceFrames: [VAPictureH264; 16],
    pub coded_buf: VABufferID,
    pub pic_parameter_set_id: u8,
    pub seq_parameter_set_id: u8,
    pub last_picture: u8,
    pub frame_num: u16,
    pub pic_init_qp: u8,
    pub num_ref_idx_l0_active_minus1: u8,
    pub num_ref_idx_l1_active_minus1: u8,
    pub chroma_qp_index_offset: i8,
    pub second_chroma_qp_index_offset: i8,
    pub pic_fields: u32,
    pub va_reserved: [u32; VA_PADDING_LOW],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAEncSliceParameterBufferH264 {
    pub macroblock_address: u32,
    pub num_macroblocks: u32,
    pub macroblock_info: VABufferID,
    pub slice_type: u8,
    pub pic_parameter_set_id: u8,
    pub idr_pic_id: u16,
    pub pic_order_cnt_lsb: u16,
    pub delta_pic_order_cnt_bottom: i32,
    pub delta_pic_order_cnt: [i32; 2],
    pub direct_spatial_mv_pred_flag: u8,
    pub num_ref_idx_active_override_flag: u8,
    pub num_ref_idx_l0_active_minus1: u8,
    pub num_ref_idx_l1_active_minus1: u8,
    pub RefPicList0: [VAPictureH264; 32],
    pub RefPicList1: [VAPictureH264; 32],
    pub luma_log2_weight_denom: u8,
    pub chroma_log2_weight_denom: u8,
    pub luma_weight_l0_flag: u8,
    pub luma_weight_l0: [i16; 32],
    pub luma_offset_l0: [i16; 32],
    pub chroma_weight_l0_flag: u8,
    pub chroma_weight_l0: [[i16; 2]; 32],
    pub chroma_offset_l0: [[i16; 2]; 32],
    pub luma_weight_l1_flag: u8,
    pub luma_weight_l1: [i16; 32],
    pub luma_offset_l1: [i16; 32],
    pub chroma_weight_l1_flag: u8,
    pub chroma_weight_l1: [[i16; 2]; 32],
    pub chroma_offset_l1: [[i16; 2]; 32],
    pub cabac_init_idc: u8,
    pub slice_qp_delta: i8,
    pub disable_deblocking_filter_idc: u8,
    pub slice_alpha_c0_offset_div2: i8,
    pub slice_beta_offset_div2: i8,
    pub va_reserved: [u32; VA_PADDING_LOW],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAPictureHEVC {
    pub picture_id: VASurfaceID,
    pub pic_order_cnt: i32,
    pub flags: u32,
    pub va_reserved: [u32; VA_PADDING_LOW],
}

pub const VA_PICTURE_HEVC_INVALID: u32 = 0x0000_0001;
pub const VA_PICTURE_HEVC_LONG_TERM_REFERENCE: u32 = 0x0000_0008;
pub const VA_PICTURE_HEVC_RPS_ST_CURR_BEFORE: u32 = 0x0000_0010;
pub const VA_PICTURE_HEVC_RPS_ST_CURR_AFTER: u32 = 0x0000_0020;
pub const VA_PICTURE_HEVC_RPS_LT_CURR: u32 = 0x0000_0040;

impl VAPictureHEVC {
    pub fn invalid() -> Self {
        VAPictureHEVC {
            picture_id: VA_INVALID_SURFACE,
            pic_order_cnt: 0,
            flags: VA_PICTURE_HEVC_INVALID,
            va_reserved: [0; VA_PADDING_LOW],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAPictureParameterBufferHEVC {
    pub CurrPic: VAPictureHEVC,
    pub ReferenceFrames: [VAPictureHEVC; 15],
    pub pic_width_in_luma_samples: u16,
    pub pic_height_in_luma_samples: u16,
    pub pic_fields: u32,
    pub sps_max_dec_pic_buffering_minus1: u8,
    pub bit_depth_luma_minus8: u8,
    pub bit_depth_chroma_minus8: u8,
    pub pcm_sample_bit_depth_luma_minus1: u8,
    pub pcm_sample_bit_depth_chroma_minus1: u8,
    pub log2_min_luma_coding_block_size_minus3: u8,
    pub log2_diff_max_min_luma_coding_block_size: u8,
    pub log2_min_transform_block_size_minus2: u8,
    pub log2_diff_max_min_transform_block_size: u8,
    pub log2_min_pcm_luma_coding_block_size_minus3: u8,
    pub log2_diff_max_min_pcm_luma_coding_block_size: u8,
    pub max_transform_hierarchy_depth_intra: u8,
    pub max_transform_hierarchy_depth_inter: u8,
    pub init_qp_minus26: i8,
    pub diff_cu_qp_delta_depth: u8,
    pub pps_cb_qp_offset: i8,
    pub pps_cr_qp_offset: i8,
    pub log2_parallel_merge_level_minus2: u8,
    pub num_tile_columns_minus1: u8,
    pub num_tile_rows_minus1: u8,
    pub column_width_minus1: [u16; 19],
    pub row_height_minus1: [u16; 21],
    pub slice_parsing_fields: u32,
    pub log2_max_pic_order_cnt_lsb_minus4: u8,
    pub num_short_term_ref_pic_sets: u8,
    pub num_long_term_ref_pic_sps: u8,
    pub num_ref_idx_l0_default_active_minus1: u8,
    pub num_ref_idx_l1_default_active_minus1: u8,
    pub pps_beta_offset_div2: i8,
    pub pps_tc_offset_div2: i8,
    pub num_extra_slice_header_bits: u8,
    pub st_rps_bits: u32,
    pub va_reserved: [u32; VA_PADDING_MEDIUM],
}

pub const HEVC_PIC_CHROMA_FORMAT_IDC_SHIFT: u32 = 0; // 2 bits
pub const HEVC_PIC_SEPARATE_COLOUR_PLANE_FLAG: u32 = 1 << 2;
pub const HEVC_PIC_PCM_ENABLED_FLAG: u32 = 1 << 3;
pub const HEVC_PIC_SCALING_LIST_ENABLED_FLAG: u32 = 1 << 4;
pub const HEVC_PIC_TRANSFORM_SKIP_ENABLED_FLAG: u32 = 1 << 5;
pub const HEVC_PIC_AMP_ENABLED_FLAG: u32 = 1 << 6;
pub const HEVC_PIC_STRONG_INTRA_SMOOTHING_ENABLED_FLAG: u32 = 1 << 7;
pub const HEVC_PIC_SIGN_DATA_HIDING_ENABLED_FLAG: u32 = 1 << 8;
pub const HEVC_PIC_CONSTRAINED_INTRA_PRED_FLAG: u32 = 1 << 9;
pub const HEVC_PIC_CU_QP_DELTA_ENABLED_FLAG: u32 = 1 << 10;
pub const HEVC_PIC_WEIGHTED_PRED_FLAG: u32 = 1 << 11;
pub const HEVC_PIC_WEIGHTED_BIPRED_FLAG: u32 = 1 << 12;
pub const HEVC_PIC_TRANSQUANT_BYPASS_ENABLED_FLAG: u32 = 1 << 13;
pub const HEVC_PIC_TILES_ENABLED_FLAG: u32 = 1 << 14;
pub const HEVC_PIC_ENTROPY_CODING_SYNC_ENABLED_FLAG: u32 = 1 << 15;
pub const HEVC_PIC_PPS_LOOP_FILTER_ACROSS_SLICES_ENABLED_FLAG: u32 = 1 << 16;
pub const HEVC_PIC_LOOP_FILTER_ACROSS_TILES_ENABLED_FLAG: u32 = 1 << 17;
pub const HEVC_PIC_PCM_LOOP_FILTER_DISABLED_FLAG: u32 = 1 << 18;
pub const HEVC_PIC_NO_PIC_REORDERING_FLAG: u32 = 1 << 19;
pub const HEVC_PIC_NO_BI_PRED_FLAG: u32 = 1 << 20;

pub const HEVC_SLP_LISTS_MODIFICATION_PRESENT_FLAG: u32 = 1 << 0;
pub const HEVC_SLP_LONG_TERM_REF_PICS_PRESENT_FLAG: u32 = 1 << 1;
pub const HEVC_SLP_SPS_TEMPORAL_MVP_ENABLED_FLAG: u32 = 1 << 2;
pub const HEVC_SLP_CABAC_INIT_PRESENT_FLAG: u32 = 1 << 3;
pub const HEVC_SLP_OUTPUT_FLAG_PRESENT_FLAG: u32 = 1 << 4;
pub const HEVC_SLP_DEPENDENT_SLICE_SEGMENTS_ENABLED_FLAG: u32 = 1 << 5;
pub const HEVC_SLP_PPS_SLICE_CHROMA_QP_OFFSETS_PRESENT_FLAG: u32 = 1 << 6;
pub const HEVC_SLP_SAMPLE_ADAPTIVE_OFFSET_ENABLED_FLAG: u32 = 1 << 7;
pub const HEVC_SLP_DEBLOCKING_FILTER_OVERRIDE_ENABLED_FLAG: u32 = 1 << 8;
pub const HEVC_SLP_PPS_DISABLE_DEBLOCKING_FILTER_FLAG: u32 = 1 << 9;
pub const HEVC_SLP_SLICE_SEGMENT_HEADER_EXTENSION_PRESENT_FLAG: u32 = 1 << 10;
pub const HEVC_SLP_RAP_PIC_FLAG: u32 = 1 << 11;
pub const HEVC_SLP_IDR_PIC_FLAG: u32 = 1 << 12;
pub const HEVC_SLP_INTRA_PIC_FLAG: u32 = 1 << 13;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VASliceParameterBufferHEVC {
    pub slice_data_size: u32,
    pub slice_data_offset: u32,
    pub slice_data_flag: u32,
    pub slice_data_byte_offset: u32,
    pub slice_segment_address: u32,
    /// Indices into `ReferenceFrames[]`, 0xFF unused.
    pub RefPicList: [[u8; 15]; 2],
    pub long_slice_flags: u32,
    pub collocated_ref_idx: u8,
    pub num_ref_idx_l0_active_minus1: u8,
    pub num_ref_idx_l1_active_minus1: u8,
    pub slice_qp_delta: i8,
    pub slice_cb_qp_offset: i8,
    pub slice_cr_qp_offset: i8,
    pub slice_beta_offset_div2: i8,
    pub slice_tc_offset_div2: i8,
    pub luma_log2_weight_denom: u8,
    pub delta_chroma_log2_weight_denom: i8,
    pub delta_luma_weight_l0: [i8; 15],
    pub luma_offset_l0: [i8; 15],
    pub delta_chroma_weight_l0: [[i8; 2]; 15],
    pub ChromaOffsetL0: [[i8; 2]; 15],
    pub delta_luma_weight_l1: [i8; 15],
    pub luma_offset_l1: [i8; 15],
    pub delta_chroma_weight_l1: [[i8; 2]; 15],
    pub ChromaOffsetL1: [[i8; 2]; 15],
    pub five_minus_max_num_merge_cand: u8,
    pub num_entry_point_offsets: u16,
    pub entry_offset_to_subset_array: u16,
    pub slice_data_num_emu_prevn_bytes: u16,
    pub va_reserved: [u32; VA_PADDING_LOW - 2],
}

/// va_dec_hevc.h, the range extension of a picture, `range_extension_pic_fields` bits 0 to 8
/// the SPS's flags, 9 cross_component_prediction, 10 chroma_qp_offset_list.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct VAPictureParameterBufferHEVCRext {
    pub range_extension_pic_fields: u32,
    pub diff_cu_chroma_qp_offset_depth: u8,
    pub chroma_qp_offset_list_len_minus1: u8,
    pub log2_sao_offset_scale_luma: u8,
    pub log2_sao_offset_scale_chroma: u8,
    pub log2_max_transform_skip_block_size_minus2: u8,
    pub cb_qp_offset_list: [i8; 6],
    pub cr_qp_offset_list: [i8; 6],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAPictureParameterBufferHEVCScc {
    pub screen_content_pic_fields: u32,
    pub palette_max_size: u8,
    pub delta_palette_max_predictor_size: u8,
    pub predictor_palette_size: u8,
    pub predictor_palette_entries: [[u16; 128]; 3],
    pub pps_act_y_qp_offset_plus5: i8,
    pub pps_act_cb_qp_offset_plus5: i8,
    pub pps_act_cr_qp_offset_plus3: i8,
}

/// The picture parameters a range extensions profile takes in place of the base ones.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAPictureParameterBufferHEVCExtension {
    pub base: VAPictureParameterBufferHEVC,
    pub rext: VAPictureParameterBufferHEVCRext,
    pub scc: VAPictureParameterBufferHEVCScc,
}

impl VAPictureParameterBufferHEVCScc {
    pub fn zeroed() -> Self {
        // SAFETY: integers and integer arrays, all zero is a valid value of each.
        unsafe { std::mem::zeroed() }
    }
}

/// The weighted prediction offsets a range extensions profile reads, wider than the base's.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct VASliceParameterBufferHEVCRext {
    pub luma_offset_l0: [i16; 15],
    pub ChromaOffsetL0: [[i16; 2]; 15],
    pub luma_offset_l1: [i16; 15],
    pub ChromaOffsetL1: [[i16; 2]; 15],
    /// Bit 0 cu_chroma_qp_offset_enabled_flag, bit 1 use_integer_mv_flag.
    pub slice_ext_flags: u32,
    pub slice_act_y_qp_offset: i8,
    pub slice_act_cb_qp_offset: i8,
    pub slice_act_cr_qp_offset: i8,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VASliceParameterBufferHEVCExtension {
    pub base: VASliceParameterBufferHEVC,
    pub rext: VASliceParameterBufferHEVCRext,
}

pub const HEVC_LSF_LAST_SLICE_OF_PIC: u32 = 1 << 0;
pub const HEVC_LSF_DEPENDENT_SLICE_SEGMENT_FLAG: u32 = 1 << 1;
pub const HEVC_LSF_SLICE_TYPE_SHIFT: u32 = 2; // 2 bits
pub const HEVC_LSF_COLOR_PLANE_ID_SHIFT: u32 = 4; // 2 bits
pub const HEVC_LSF_SLICE_SAO_LUMA_FLAG: u32 = 1 << 6;
pub const HEVC_LSF_SLICE_SAO_CHROMA_FLAG: u32 = 1 << 7;
pub const HEVC_LSF_MVD_L1_ZERO_FLAG: u32 = 1 << 8;
pub const HEVC_LSF_CABAC_INIT_FLAG: u32 = 1 << 9;
pub const HEVC_LSF_SLICE_TEMPORAL_MVP_ENABLED_FLAG: u32 = 1 << 10;
pub const HEVC_LSF_SLICE_DEBLOCKING_FILTER_DISABLED_FLAG: u32 = 1 << 11;
pub const HEVC_LSF_COLLOCATED_FROM_L0_FLAG: u32 = 1 << 12;
pub const HEVC_LSF_SLICE_LOOP_FILTER_ACROSS_SLICES_ENABLED_FLAG: u32 = 1 << 13;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAIQMatrixBufferHEVC {
    pub ScalingList4x4: [[u8; 16]; 6],
    pub ScalingList8x8: [[u8; 64]; 6],
    pub ScalingList16x16: [[u8; 64]; 6],
    pub ScalingList32x32: [[u8; 64]; 2],
    pub ScalingListDC16x16: [u8; 6],
    pub ScalingListDC32x32: [u8; 2],
    pub va_reserved: [u32; VA_PADDING_LOW],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAEncSequenceParameterBufferHEVC {
    pub general_profile_idc: u8,
    pub general_level_idc: u8,
    pub general_tier_flag: u8,
    pub intra_period: u32,
    pub intra_idr_period: u32,
    pub ip_period: u32,
    pub bits_per_second: u32,
    pub pic_width_in_luma_samples: u16,
    pub pic_height_in_luma_samples: u16,
    pub seq_fields: u32,
    pub log2_min_luma_coding_block_size_minus3: u8,
    pub log2_diff_max_min_luma_coding_block_size: u8,
    pub log2_min_transform_block_size_minus2: u8,
    pub log2_diff_max_min_transform_block_size: u8,
    pub max_transform_hierarchy_depth_inter: u8,
    pub max_transform_hierarchy_depth_intra: u8,
    pub pcm_sample_bit_depth_luma_minus1: u32,
    pub pcm_sample_bit_depth_chroma_minus1: u32,
    pub log2_min_pcm_luma_coding_block_size_minus3: u32,
    pub log2_max_pcm_luma_coding_block_size_minus3: u32,
    pub vui_parameters_present_flag: u8,
    pub vui_fields: u32,
    pub aspect_ratio_idc: u8,
    pub sar_width: u32,
    pub sar_height: u32,
    pub vui_num_units_in_tick: u32,
    pub vui_time_scale: u32,
    pub min_spatial_segmentation_idc: u16,
    pub max_bytes_per_pic_denom: u8,
    pub max_bits_per_min_cu_denom: u8,
    pub scc_fields: u32,
    pub va_reserved: [u32; VA_PADDING_MEDIUM - 1],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAEncPictureParameterBufferHEVC {
    pub decoded_curr_pic: VAPictureHEVC,
    pub reference_frames: [VAPictureHEVC; 15],
    pub coded_buf: VABufferID,
    pub collocated_ref_pic_index: u8,
    pub last_picture: u8,
    pub pic_init_qp: u8,
    pub diff_cu_qp_delta_depth: u8,
    pub pps_cb_qp_offset: i8,
    pub pps_cr_qp_offset: i8,
    pub num_tile_columns_minus1: u8,
    pub num_tile_rows_minus1: u8,
    pub column_width_minus1: [u8; 19],
    pub row_height_minus1: [u8; 21],
    pub log2_parallel_merge_level_minus2: u8,
    pub ctu_max_bitsize_allowed: u8,
    pub num_ref_idx_l0_default_active_minus1: u8,
    pub num_ref_idx_l1_default_active_minus1: u8,
    pub slice_pic_parameter_set_id: u8,
    pub nal_unit_type: u8,
    pub pic_fields: u32,
    pub hierarchical_level_plus1: u8,
    pub va_byte_reserved: u8,
    pub scc_fields: u16,
    pub va_reserved: [u32; VA_PADDING_HIGH - 1],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAEncSliceParameterBufferHEVC {
    pub slice_segment_address: u32,
    pub num_ctu_in_slice: u32,
    pub slice_type: u8,
    pub slice_pic_parameter_set_id: u8,
    pub num_ref_idx_l0_active_minus1: u8,
    pub num_ref_idx_l1_active_minus1: u8,
    pub ref_pic_list0: [VAPictureHEVC; 15],
    pub ref_pic_list1: [VAPictureHEVC; 15],
    pub luma_log2_weight_denom: u8,
    pub delta_chroma_log2_weight_denom: i8,
    pub delta_luma_weight_l0: [i8; 15],
    pub luma_offset_l0: [i8; 15],
    pub delta_chroma_weight_l0: [[i8; 2]; 15],
    pub chroma_offset_l0: [[i8; 2]; 15],
    pub delta_luma_weight_l1: [i8; 15],
    pub luma_offset_l1: [i8; 15],
    pub delta_chroma_weight_l1: [[i8; 2]; 15],
    pub chroma_offset_l1: [[i8; 2]; 15],
    pub max_num_merge_cand: u8,
    pub slice_qp_delta: i8,
    pub slice_cb_qp_offset: i8,
    pub slice_cr_qp_offset: i8,
    pub slice_beta_offset_div2: i8,
    pub slice_tc_offset_div2: i8,
    pub slice_fields: u32,
    pub pred_weight_table_bit_offset: u32,
    pub pred_weight_table_bit_length: u32,
    pub va_reserved: [u32; VA_PADDING_MEDIUM - 2],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAEncSequenceParameterBufferVP8 {
    pub frame_width: u32,
    pub frame_height: u32,
    pub frame_width_scale: u32,
    pub frame_height_scale: u32,
    pub error_resilient: u32,
    pub kf_auto: u32,
    pub kf_min_dist: u32,
    pub kf_max_dist: u32,
    pub bits_per_second: u32,
    pub intra_period: u32,
    pub reference_frames: [VASurfaceID; 4],
    pub va_reserved: [u32; VA_PADDING_LOW],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAEncPictureParameterBufferVP8 {
    pub reconstructed_frame: VASurfaceID,
    pub ref_last_frame: VASurfaceID,
    pub ref_gf_frame: VASurfaceID,
    pub ref_arf_frame: VASurfaceID,
    pub coded_buf: VABufferID,
    pub ref_flags: u32,
    pub pic_flags: u32,
    pub loop_filter_level: [i8; 4],
    pub ref_lf_delta: [i8; 4],
    pub mode_lf_delta: [i8; 4],
    pub sharpness_level: u8,
    pub clamp_qindex_high: u8,
    pub clamp_qindex_low: u8,
    pub va_reserved: [u32; VA_PADDING_LOW],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VADecPictureParameterBufferVP9 {
    pub frame_width: u16,
    pub frame_height: u16,
    pub reference_frames: [VASurfaceID; 8],
    pub pic_fields: u32,
    pub filter_level: u8,
    pub sharpness_level: u8,
    pub log2_tile_rows: u8,
    pub log2_tile_columns: u8,
    pub frame_header_length_in_bytes: u8,
    /// `header_size_in_bytes`, the compressed header's length.
    pub first_partition_size: u16,
    pub mb_segment_tree_probs: [u8; 7],
    pub segment_pred_probs: [u8; 3],
    pub profile: u8,
    pub bit_depth: u8,
    pub va_reserved: [u32; VA_PADDING_MEDIUM],
}

pub const VP9_PIC_SUBSAMPLING_X: u32 = 1 << 0;
pub const VP9_PIC_SUBSAMPLING_Y: u32 = 1 << 1;
pub const VP9_PIC_FRAME_TYPE: u32 = 1 << 2;
pub const VP9_PIC_SHOW_FRAME: u32 = 1 << 3;
pub const VP9_PIC_ERROR_RESILIENT_MODE: u32 = 1 << 4;
pub const VP9_PIC_INTRA_ONLY: u32 = 1 << 5;
pub const VP9_PIC_ALLOW_HIGH_PRECISION_MV: u32 = 1 << 6;
pub const VP9_PIC_MCOMP_FILTER_TYPE_SHIFT: u32 = 7; // 3 bits
pub const VP9_PIC_FRAME_PARALLEL_DECODING_MODE: u32 = 1 << 10;
pub const VP9_PIC_RESET_FRAME_CONTEXT_SHIFT: u32 = 11; // 2 bits
pub const VP9_PIC_REFRESH_FRAME_CONTEXT: u32 = 1 << 13;
pub const VP9_PIC_FRAME_CONTEXT_IDX_SHIFT: u32 = 14; // 2 bits
pub const VP9_PIC_SEGMENTATION_ENABLED: u32 = 1 << 16;
pub const VP9_PIC_SEGMENTATION_TEMPORAL_UPDATE: u32 = 1 << 17;
pub const VP9_PIC_SEGMENTATION_UPDATE_MAP: u32 = 1 << 18;
pub const VP9_PIC_LAST_REF_FRAME_SHIFT: u32 = 19; // 3 bits
pub const VP9_PIC_LAST_REF_FRAME_SIGN_BIAS: u32 = 1 << 22;
pub const VP9_PIC_GOLDEN_REF_FRAME_SHIFT: u32 = 23; // 3 bits
pub const VP9_PIC_GOLDEN_REF_FRAME_SIGN_BIAS: u32 = 1 << 26;
pub const VP9_PIC_ALT_REF_FRAME_SHIFT: u32 = 27; // 3 bits
pub const VP9_PIC_ALT_REF_FRAME_SIGN_BIAS: u32 = 1 << 30;
pub const VP9_PIC_LOSSLESS_FLAG: u32 = 1 << 31;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VASegmentParameterVP9 {
    pub segment_flags: u16,
    pub filter_level: [[u8; 2]; 4],
    pub luma_ac_quant_scale: i16,
    pub luma_dc_quant_scale: i16,
    pub chroma_ac_quant_scale: i16,
    pub chroma_dc_quant_scale: i16,
    pub va_reserved: [u32; VA_PADDING_LOW],
}

pub const VP9_SEG_REFERENCE_ENABLED: u16 = 1 << 0;
pub const VP9_SEG_REFERENCE_SHIFT: u16 = 1; // 2 bits
pub const VP9_SEG_REFERENCE_SKIPPED: u16 = 1 << 3;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VASliceParameterBufferVP9 {
    pub slice_data_size: u32,
    pub slice_data_offset: u32,
    pub slice_data_flag: u32,
    pub seg_param: [VASegmentParameterVP9; 8],
    pub va_reserved: [u32; VA_PADDING_LOW],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VASegmentationStructAV1 {
    pub segment_info_fields: u32,
    pub feature_data: [[i16; 8]; 8],
    pub feature_mask: [u8; 8],
    pub va_reserved: [u32; VA_PADDING_LOW],
}

pub const AV1_SEG_ENABLED: u32 = 1 << 0;
pub const AV1_SEG_UPDATE_MAP: u32 = 1 << 1;
pub const AV1_SEG_TEMPORAL_UPDATE: u32 = 1 << 2;
pub const AV1_SEG_UPDATE_DATA: u32 = 1 << 3;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAFilmGrainStructAV1 {
    pub film_grain_info_fields: u32,
    pub grain_seed: u16,
    pub num_y_points: u8,
    pub point_y_value: [u8; 14],
    pub point_y_scaling: [u8; 14],
    pub num_cb_points: u8,
    pub point_cb_value: [u8; 10],
    pub point_cb_scaling: [u8; 10],
    pub num_cr_points: u8,
    pub point_cr_value: [u8; 10],
    pub point_cr_scaling: [u8; 10],
    pub ar_coeffs_y: [i8; 24],
    pub ar_coeffs_cb: [i8; 25],
    pub ar_coeffs_cr: [i8; 25],
    pub cb_mult: u8,
    pub cb_luma_mult: u8,
    pub cb_offset: u16,
    pub cr_mult: u8,
    pub cr_luma_mult: u8,
    pub cr_offset: u16,
    pub va_reserved: [u32; VA_PADDING_LOW],
}

pub const AV1_FG_APPLY_GRAIN: u32 = 1 << 0;
pub const AV1_FG_CHROMA_SCALING_FROM_LUMA: u32 = 1 << 1;
pub const AV1_FG_GRAIN_SCALING_MINUS_8_SHIFT: u32 = 2; // 2 bits
pub const AV1_FG_AR_COEFF_LAG_SHIFT: u32 = 4; // 2 bits
pub const AV1_FG_AR_COEFF_SHIFT_MINUS_6_SHIFT: u32 = 6; // 2 bits
pub const AV1_FG_GRAIN_SCALE_SHIFT_SHIFT: u32 = 8; // 2 bits
pub const AV1_FG_OVERLAP_FLAG: u32 = 1 << 10;
pub const AV1_FG_CLIP_TO_RESTRICTED_RANGE: u32 = 1 << 11;

pub type VAAV1TransformationType = c_int;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAWarpedMotionParamsAV1 {
    pub wmtype: VAAV1TransformationType,
    pub wmmat: [i32; 8],
    pub invalid: u8,
    pub va_reserved: [u32; VA_PADDING_LOW],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VADecPictureParameterBufferAV1 {
    pub profile: u8,
    pub order_hint_bits_minus_1: u8,
    /// 0, 1, 2 for 8, 10, 12 bits.
    pub bit_depth_idx: u8,
    pub matrix_coefficients: u8,
    pub seq_info_fields: u32,
    pub current_frame: VASurfaceID,
    /// Where the grained picture goes with `apply_grain`, else `current_frame`.
    pub current_display_picture: VASurfaceID,
    pub anchor_frames_num: u8,
    pub anchor_frames_list: *mut VASurfaceID,
    pub frame_width_minus1: u16,
    pub frame_height_minus1: u16,
    pub output_frame_width_in_tiles_minus_1: u16,
    pub output_frame_height_in_tiles_minus_1: u16,
    pub ref_frame_map: [VASurfaceID; 8],
    pub ref_frame_idx: [u8; 7],
    pub primary_ref_frame: u8,
    pub order_hint: u8,
    pub seg_info: VASegmentationStructAV1,
    pub film_grain_info: VAFilmGrainStructAV1,
    pub tile_cols: u8,
    pub tile_rows: u8,
    pub width_in_sbs_minus_1: [u16; 63],
    pub height_in_sbs_minus_1: [u16; 63],
    pub tile_count_minus_1: u16,
    pub context_update_tile_id: u16,
    pub pic_info_fields: u32,
    pub superres_scale_denominator: u8,
    pub interp_filter: u8,
    pub filter_level: [u8; 2],
    pub filter_level_u: u8,
    pub filter_level_v: u8,
    pub loop_filter_info_fields: u8,
    pub ref_deltas: [i8; 8],
    pub mode_deltas: [i8; 2],
    pub base_qindex: u8,
    pub y_dc_delta_q: i8,
    pub u_dc_delta_q: i8,
    pub u_ac_delta_q: i8,
    pub v_dc_delta_q: i8,
    pub v_ac_delta_q: i8,
    pub qmatrix_fields: u16,
    pub mode_control_fields: u32,
    pub cdef_damping_minus_3: u8,
    pub cdef_bits: u8,
    /// `pri << 2 | sec`, the coded strengths.
    pub cdef_y_strengths: [u8; 8],
    pub cdef_uv_strengths: [u8; 8],
    pub loop_restoration_fields: u16,
    pub wm: [VAWarpedMotionParamsAV1; 7],
    pub va_reserved: [u32; VA_PADDING_MEDIUM],
}

pub const AV1_SEQ_STILL_PICTURE: u32 = 1 << 0;
pub const AV1_SEQ_USE_128X128_SUPERBLOCK: u32 = 1 << 1;
pub const AV1_SEQ_ENABLE_FILTER_INTRA: u32 = 1 << 2;
pub const AV1_SEQ_ENABLE_INTRA_EDGE_FILTER: u32 = 1 << 3;
pub const AV1_SEQ_ENABLE_INTERINTRA_COMPOUND: u32 = 1 << 4;
pub const AV1_SEQ_ENABLE_MASKED_COMPOUND: u32 = 1 << 5;
pub const AV1_SEQ_ENABLE_DUAL_FILTER: u32 = 1 << 6;
pub const AV1_SEQ_ENABLE_ORDER_HINT: u32 = 1 << 7;
pub const AV1_SEQ_ENABLE_JNT_COMP: u32 = 1 << 8;
pub const AV1_SEQ_ENABLE_CDEF: u32 = 1 << 9;
pub const AV1_SEQ_MONO_CHROME: u32 = 1 << 10;
pub const AV1_SEQ_COLOR_RANGE: u32 = 1 << 11;
pub const AV1_SEQ_SUBSAMPLING_X: u32 = 1 << 12;
pub const AV1_SEQ_SUBSAMPLING_Y: u32 = 1 << 13;
pub const AV1_SEQ_CHROMA_SAMPLE_POSITION: u32 = 1 << 14;
pub const AV1_SEQ_FILM_GRAIN_PARAMS_PRESENT: u32 = 1 << 15;

pub const AV1_PIC_FRAME_TYPE_SHIFT: u32 = 0; // 2 bits
pub const AV1_PIC_SHOW_FRAME: u32 = 1 << 2;
pub const AV1_PIC_SHOWABLE_FRAME: u32 = 1 << 3;
pub const AV1_PIC_ERROR_RESILIENT_MODE: u32 = 1 << 4;
pub const AV1_PIC_DISABLE_CDF_UPDATE: u32 = 1 << 5;
pub const AV1_PIC_ALLOW_SCREEN_CONTENT_TOOLS: u32 = 1 << 6;
pub const AV1_PIC_FORCE_INTEGER_MV: u32 = 1 << 7;
pub const AV1_PIC_ALLOW_INTRABC: u32 = 1 << 8;
pub const AV1_PIC_USE_SUPERRES: u32 = 1 << 9;
pub const AV1_PIC_ALLOW_HIGH_PRECISION_MV: u32 = 1 << 10;
pub const AV1_PIC_IS_MOTION_MODE_SWITCHABLE: u32 = 1 << 11;
pub const AV1_PIC_USE_REF_FRAME_MVS: u32 = 1 << 12;
pub const AV1_PIC_DISABLE_FRAME_END_UPDATE_CDF: u32 = 1 << 13;
pub const AV1_PIC_UNIFORM_TILE_SPACING_FLAG: u32 = 1 << 14;
pub const AV1_PIC_ALLOW_WARPED_MOTION: u32 = 1 << 15;
pub const AV1_PIC_LARGE_SCALE_TILE: u32 = 1 << 16;

pub const AV1_LF_SHARPNESS_SHIFT: u8 = 0; // 3 bits
pub const AV1_LF_MODE_REF_DELTA_ENABLED: u8 = 1 << 3;
pub const AV1_LF_MODE_REF_DELTA_UPDATE: u8 = 1 << 4;

pub const AV1_QM_USING_QMATRIX: u16 = 1 << 0;
pub const AV1_QM_Y_SHIFT: u16 = 1; // 4 bits
pub const AV1_QM_U_SHIFT: u16 = 5; // 4 bits
pub const AV1_QM_V_SHIFT: u16 = 9; // 4 bits

pub const AV1_MODE_DELTA_Q_PRESENT: u32 = 1 << 0;
pub const AV1_MODE_LOG2_DELTA_Q_RES_SHIFT: u32 = 1; // 2 bits
pub const AV1_MODE_DELTA_LF_PRESENT: u32 = 1 << 3;
pub const AV1_MODE_LOG2_DELTA_LF_RES_SHIFT: u32 = 4; // 2 bits
pub const AV1_MODE_DELTA_LF_MULTI: u32 = 1 << 6;
pub const AV1_MODE_TX_MODE_SHIFT: u32 = 7; // 2 bits
pub const AV1_MODE_REFERENCE_SELECT: u32 = 1 << 9;
pub const AV1_MODE_REDUCED_TX_SET_USED: u32 = 1 << 10;
pub const AV1_MODE_SKIP_MODE_PRESENT: u32 = 1 << 11;

pub const AV1_LR_Y_TYPE_SHIFT: u16 = 0; // 2 bits
pub const AV1_LR_CB_TYPE_SHIFT: u16 = 2; // 2 bits
pub const AV1_LR_CR_TYPE_SHIFT: u16 = 4; // 2 bits
pub const AV1_LR_UNIT_SHIFT_SHIFT: u16 = 6; // 2 bits
pub const AV1_LR_UV_SHIFT: u16 = 1 << 8;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VASliceParameterBufferAV1 {
    pub slice_data_size: u32,
    pub slice_data_offset: u32,
    pub slice_data_flag: u32,
    pub tile_row: u16,
    pub tile_column: u16,
    pub tg_start: u16,
    pub tg_end: u16,
    pub anchor_frame_idx: u8,
    pub tile_idx_in_tile_list: u16,
    pub va_reserved: [u32; VA_PADDING_LOW],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAQMatrixBufferVP8 {
    pub quantization_index: [u16; 4],
    pub quantization_index_delta: [i16; 5],
    pub va_reserved: [u32; VA_PADDING_LOW],
}

pub const VA_FILTER_SCALING_HQ: u32 = 0x0000_0200;

pub type VAProcColorStandardType = c_int;
pub const VAProcColorStandardNone: VAProcColorStandardType = 0;
pub const VAProcColorStandardBT601: VAProcColorStandardType = 1;
pub const VAProcColorStandardBT709: VAProcColorStandardType = 2;
pub const VAProcColorStandardBT2020: VAProcColorStandardType = 12;
pub const VAProcColorStandardExplicit: VAProcColorStandardType = 13;

pub type VAProcFilterType = c_int;
pub const VAProcFilterHighDynamicRangeToneMapping: VAProcFilterType = 8;
pub const VAProcFilterCount: usize = 10;

pub type VAProcHighDynamicRangeMetadataType = c_int;
pub const VAProcHighDynamicRangeMetadataHDR10: VAProcHighDynamicRangeMetadataType = 1;

pub const VA_TONE_MAPPING_HDR_TO_HDR: u16 = 0x0001;
pub const VA_TONE_MAPPING_HDR_TO_SDR: u16 = 0x0002;
pub const VA_TONE_MAPPING_HDR_TO_EDR: u16 = 0x0004;
pub const VA_TONE_MAPPING_SDR_TO_HDR: u16 = 0x0008;

/// Primaries are green, blue, red in 0.00002 steps, luminances in 0.0001 cd/m2, light levels in
/// cd/m2.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct VAHdrMetaDataHDR10 {
    pub display_primaries_x: [u16; 3],
    pub display_primaries_y: [u16; 3],
    pub white_point_x: u16,
    pub white_point_y: u16,
    pub max_display_mastering_luminance: u32,
    pub min_display_mastering_luminance: u32,
    pub max_content_light_level: u16,
    pub max_pic_average_light_level: u16,
    pub reserved: [u16; VA_PADDING_HIGH],
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct VAHdrMetaData {
    pub metadata_type: VAProcHighDynamicRangeMetadataType,
    pub metadata: *mut c_void,
    pub metadata_size: u32,
    pub reserved: [u32; VA_PADDING_LOW],
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct VAProcFilterParameterBufferHDRToneMapping {
    pub type_: VAProcFilterType,
    pub data: VAHdrMetaData,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct VAProcFilterCapHighDynamicRange {
    pub metadata_type: VAProcHighDynamicRangeMetadataType,
    pub caps_flag: u16,
    pub va_reserved: [u16; VA_PADDING_HIGH],
}

pub const VA_SOURCE_RANGE_UNKNOWN: u8 = 0;
pub const VA_SOURCE_RANGE_REDUCED: u8 = 1;
pub const VA_SOURCE_RANGE_FULL: u8 = 2;

#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VARectangle {
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct VAProcColorProperties {
    pub chroma_sample_location: u8,
    pub color_range: u8,
    pub colour_primaries: u8,
    pub transfer_characteristics: u8,
    pub matrix_coefficients: u8,
    pub reserved: [u8; 3],
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct VAProcPipelineParameterBuffer {
    pub surface: VASurfaceID,
    pub surface_region: *const VARectangle,
    pub surface_color_standard: VAProcColorStandardType,
    pub output_region: *const VARectangle,
    pub output_background_color: u32,
    pub output_color_standard: VAProcColorStandardType,
    pub pipeline_flags: u32,
    pub filter_flags: u32,
    pub filters: *mut VABufferID,
    pub num_filters: u32,
    pub forward_references: *mut VASurfaceID,
    pub num_forward_references: u32,
    pub backward_references: *mut VASurfaceID,
    pub num_backward_references: u32,
    pub rotation_state: u32,
    pub blend_state: *const c_void,
    pub mirror_state: u32,
    pub additional_outputs: *mut VASurfaceID,
    pub num_additional_outputs: u32,
    pub input_surface_flag: u32,
    pub output_surface_flag: u32,
    pub input_color_properties: VAProcColorProperties,
    pub output_color_properties: VAProcColorProperties,
    pub processing_mode: i32,
    pub output_hdr_metadata: *mut c_void,
    pub va_reserved: [u32; VA_PADDING_HIGH],
}

impl VAProcPipelineParameterBuffer {
    pub fn zeroed() -> Self {
        // SAFETY: integers and raw pointers, all zero is a valid value of each.
        unsafe { std::mem::zeroed() }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct VAProcPipelineCaps {
    pub pipeline_flags: u32,
    pub filter_flags: u32,
    pub num_forward_references: u32,
    pub num_backward_references: u32,
    pub input_color_standards: *mut VAProcColorStandardType,
    pub num_input_color_standards: u32,
    pub output_color_standards: *mut VAProcColorStandardType,
    pub num_output_color_standards: u32,
    pub rotation_flags: u32,
    pub blend_flags: u32,
    pub mirror_flags: u32,
    pub num_additional_outputs: u32,
    pub num_input_pixel_formats: u32,
    pub input_pixel_format: *mut u32,
    pub num_output_pixel_formats: u32,
    pub output_pixel_format: *mut u32,
    pub max_input_width: u32,
    pub max_input_height: u32,
    pub min_input_width: u32,
    pub min_input_height: u32,
    pub max_output_width: u32,
    pub max_output_height: u32,
    pub min_output_width: u32,
    pub min_output_height: u32,
    pub va_reserved: [u32; 14],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct VAEncSequenceParameterBufferVP9 {
    pub max_frame_width: u32,
    pub max_frame_height: u32,
    pub kf_auto: u32,
    pub kf_min_dist: u32,
    pub kf_max_dist: u32,
    pub bits_per_second: u32,
    pub intra_period: u32,
    pub va_reserved: [u32; VA_PADDING_LOW],
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct VAEncPictureParameterBufferVP9 {
    pub frame_width_src: u32,
    pub frame_height_src: u32,
    pub frame_width_dst: u32,
    pub frame_height_dst: u32,
    pub reconstructed_frame: VASurfaceID,
    pub reference_frames: [VASurfaceID; 8],
    pub coded_buf: VABufferID,
    pub ref_flags: u32,
    pub pic_flags: u32,
    pub refresh_frame_flags: u8,
    pub luma_ac_qindex: u8,
    pub luma_dc_qindex_delta: i8,
    pub chroma_ac_qindex_delta: i8,
    pub chroma_dc_qindex_delta: i8,
    pub filter_level: u8,
    pub sharpness_level: u8,
    pub ref_lf_delta: [i8; 4],
    pub mode_lf_delta: [i8; 2],
    pub bit_offset_ref_lf_delta: u16,
    pub bit_offset_mode_lf_delta: u16,
    pub bit_offset_lf_level: u16,
    pub bit_offset_qindex: u16,
    pub bit_offset_first_partition_size: u16,
    pub bit_offset_segmentation: u16,
    pub bit_size_segmentation: u16,
    pub log2_tile_rows: u8,
    pub log2_tile_columns: u8,
    pub skip_frame_flag: u8,
    pub number_skip_frames: u8,
    pub skip_frames_size: u32,
    pub seg_id_block_size: u8,
    pub va_reserved8: [u8; 3],
    pub va_reserved: [u32; 7],
}

pub const VP9_ENC_REF_FORCE_KF: u32 = 1 << 0;
pub const VP9_ENC_REF_FRAME_CTRL_L0_SHIFT: u32 = 1; // 3 bits
pub const VP9_ENC_REF_LAST_IDX_SHIFT: u32 = 7; // 3 bits
pub const VP9_ENC_REF_LAST_SIGN_BIAS: u32 = 1 << 10;
pub const VP9_ENC_PIC_FRAME_TYPE_INTER: u32 = 1 << 0;
pub const VP9_ENC_PIC_SHOW_FRAME: u32 = 1 << 1;

#[link(name = "va")]
#[link(name = "va-drm")]
unsafe extern "C" {
    pub fn vaGetDisplayDRM(fd: c_int) -> VADisplay;

    pub fn vaInitialize(dpy: VADisplay, major: *mut c_int, minor: *mut c_int) -> VAStatus;
    pub fn vaTerminate(dpy: VADisplay) -> VAStatus;
    pub fn vaErrorStr(error_status: VAStatus) -> *const c_char;
    pub fn vaQueryVendorString(dpy: VADisplay) -> *const c_char;
    pub fn vaMaxNumProfiles(dpy: VADisplay) -> c_int;
    pub fn vaMaxNumEntrypoints(dpy: VADisplay) -> c_int;
    pub fn vaQueryConfigProfiles(
        dpy: VADisplay,
        profile_list: *mut VAProfile,
        num_profiles: *mut c_int,
    ) -> VAStatus;
    pub fn vaQueryConfigEntrypoints(
        dpy: VADisplay,
        profile: VAProfile,
        entrypoint_list: *mut VAEntrypoint,
        num_entrypoints: *mut c_int,
    ) -> VAStatus;
    pub fn vaGetConfigAttributes(
        dpy: VADisplay,
        profile: VAProfile,
        entrypoint: VAEntrypoint,
        attrib_list: *mut VAConfigAttrib,
        num_attribs: c_int,
    ) -> VAStatus;

    pub fn vaCreateConfig(
        dpy: VADisplay,
        profile: VAProfile,
        entrypoint: VAEntrypoint,
        attrib_list: *mut VAConfigAttrib,
        num_attribs: c_int,
        config_id: *mut VAConfigID,
    ) -> VAStatus;
    pub fn vaDestroyConfig(dpy: VADisplay, config_id: VAConfigID) -> VAStatus;
    pub fn vaCreateSurfaces(
        dpy: VADisplay,
        format: u32,
        width: u32,
        height: u32,
        surfaces: *mut VASurfaceID,
        num_surfaces: u32,
        attrib_list: *mut VASurfaceAttrib,
        num_attribs: u32,
    ) -> VAStatus;
    pub fn vaDestroySurfaces(
        dpy: VADisplay,
        surfaces: *mut VASurfaceID,
        num_surfaces: c_int,
    ) -> VAStatus;
    pub fn vaCreateContext(
        dpy: VADisplay,
        config_id: VAConfigID,
        picture_width: c_int,
        picture_height: c_int,
        flag: c_int,
        render_targets: *mut VASurfaceID,
        num_render_targets: c_int,
        context: *mut VAContextID,
    ) -> VAStatus;
    pub fn vaDestroyContext(dpy: VADisplay, context: VAContextID) -> VAStatus;

    pub fn vaCreateBuffer(
        dpy: VADisplay,
        context: VAContextID,
        type_: VABufferType,
        size: u32,
        num_elements: u32,
        data: *mut c_void,
        buf_id: *mut VABufferID,
    ) -> VAStatus;
    pub fn vaDestroyBuffer(dpy: VADisplay, buffer_id: VABufferID) -> VAStatus;
    pub fn vaBeginPicture(
        dpy: VADisplay,
        context: VAContextID,
        render_target: VASurfaceID,
    ) -> VAStatus;
    pub fn vaRenderPicture(
        dpy: VADisplay,
        context: VAContextID,
        buffers: *mut VABufferID,
        num_buffers: c_int,
    ) -> VAStatus;
    pub fn vaEndPicture(dpy: VADisplay, context: VAContextID) -> VAStatus;
    pub fn vaSyncSurface(dpy: VADisplay, render_target: VASurfaceID) -> VAStatus;
    pub fn vaSyncBuffer(dpy: VADisplay, buf_id: VABufferID, timeout_ns: u64) -> VAStatus;
    pub fn vaSyncSurface2(dpy: VADisplay, surface: VASurfaceID, timeout_ns: u64) -> VAStatus;

    pub fn vaDeriveImage(dpy: VADisplay, surface: VASurfaceID, image: *mut VAImage) -> VAStatus;
    pub fn vaCreateImage(
        dpy: VADisplay,
        format: *mut VAImageFormat,
        width: c_int,
        height: c_int,
        image: *mut VAImage,
    ) -> VAStatus;
    pub fn vaGetImage(
        dpy: VADisplay,
        surface: VASurfaceID,
        x: c_int,
        y: c_int,
        width: u32,
        height: u32,
        image: VAImageID,
    ) -> VAStatus;
    pub fn vaMapBuffer(dpy: VADisplay, buf_id: VABufferID, pbuf: *mut *mut c_void) -> VAStatus;
    pub fn vaUnmapBuffer(dpy: VADisplay, buf_id: VABufferID) -> VAStatus;
    pub fn vaDestroyImage(dpy: VADisplay, image: VAImageID) -> VAStatus;

    // Fallback when the driver refuses vaDeriveImage on an input surface.
    pub fn vaPutImage(
        dpy: VADisplay,
        surface: VASurfaceID,
        image: VAImageID,
        src_x: c_int,
        src_y: c_int,
        src_width: u32,
        src_height: u32,
        dest_x: c_int,
        dest_y: c_int,
        dest_width: u32,
        dest_height: u32,
    ) -> VAStatus;

    // The surface must be synced first. The caller owns and closes each returned fd.
    pub fn vaExportSurfaceHandle(
        dpy: VADisplay,
        surface_id: VASurfaceID,
        mem_type: u32,
        flags: u32,
        descriptor: *mut c_void,
    ) -> VAStatus;

    pub fn vaQueryVideoProcFilters(
        dpy: VADisplay,
        context: VAContextID,
        filters: *mut VAProcFilterType,
        num_filters: *mut u32,
    ) -> VAStatus;
    pub fn vaQueryVideoProcFilterCaps(
        dpy: VADisplay,
        context: VAContextID,
        type_: VAProcFilterType,
        filter_caps: *mut c_void,
        num_filter_caps: *mut u32,
    ) -> VAStatus;

    pub fn vaQueryVideoProcPipelineCaps(
        dpy: VADisplay,
        context: VAContextID,
        filters: *mut VABufferID,
        num_filters: u32,
        pipeline_caps: *mut VAProcPipelineCaps,
    ) -> VAStatus;
    pub fn vaQuerySurfaceAttributes(
        dpy: VADisplay,
        config: VAConfigID,
        attrib_list: *mut VASurfaceAttrib,
        num_attribs: *mut u32,
    ) -> VAStatus;
}

#[cfg(test)]
mod layout {
    use super::*;
    use std::mem::{align_of, size_of};

    /// Sizes and offsets a C program printed against libva 2.24's va_dec_hevc.h.
    #[test]
    fn hevc_range_extension_buffers_match_libva() {
        assert_eq!(size_of::<VAPictureParameterBufferHEVC>(), 604);
        assert_eq!(size_of::<VAPictureParameterBufferHEVCRext>(), 24);
        assert_eq!(size_of::<VAPictureParameterBufferHEVCScc>(), 780);
        assert_eq!(size_of::<VAPictureParameterBufferHEVCExtension>(), 1408);
        assert_eq!(std::mem::offset_of!(VAPictureParameterBufferHEVCExtension, rext), 604);
        assert_eq!(std::mem::offset_of!(VAPictureParameterBufferHEVCExtension, scc), 628);
        assert_eq!(std::mem::offset_of!(VAPictureParameterBufferHEVCRext, cb_qp_offset_list), 9);
        assert_eq!(size_of::<VASliceParameterBufferHEVC>(), 264);
        assert_eq!(size_of::<VASliceParameterBufferHEVCRext>(), 188);
        assert_eq!(size_of::<VASliceParameterBufferHEVCExtension>(), 452);
        assert_eq!(std::mem::offset_of!(VASliceParameterBufferHEVCRext, slice_ext_flags), 180);
        assert_eq!(
            std::mem::offset_of!(VASliceParameterBufferHEVCRext, slice_act_y_qp_offset),
            184
        );
    }

    #[test]
    fn struct_sizes_match_libva_2_23() {
        assert_eq!(size_of::<VAConfigAttrib>(), 8, "VAConfigAttrib");
        assert_eq!(size_of::<VAGenericValueUnion>(), 8, "VAGenericValueUnion");
        assert_eq!(align_of::<VAGenericValueUnion>(), 8, "VAGenericValueUnion align");
        assert_eq!(size_of::<VAGenericValue>(), 16, "VAGenericValue");
        assert_eq!(size_of::<VASurfaceAttrib>(), 24, "VASurfaceAttrib");
        assert_eq!(size_of::<VAImageFormat>(), 48, "VAImageFormat");
        assert_eq!(size_of::<VAImage>(), 120, "VAImage");
        assert_eq!(size_of::<VAPictureH264>(), 36, "VAPictureH264");
        assert_eq!(size_of::<VAPictureParameterBufferH264>(), 672, "VAPictureParameterBufferH264");
        assert_eq!(size_of::<VAIQMatrixBufferH264>(), 240, "VAIQMatrixBufferH264");
        assert_eq!(size_of::<VASliceParameterBufferH264>(), 3128, "VASliceParameterBufferH264");
        assert_eq!(align_of::<VASliceParameterBufferH264>(), 4);
    }

    #[test]
    fn encode_struct_sizes_match_libva_2_23() {
        assert_eq!(size_of::<VACodedBufferSegment>(), 48, "VACodedBufferSegment");
        assert_eq!(align_of::<VACodedBufferSegment>(), 8);
        assert_eq!(
            size_of::<VAEncPackedHeaderParameterBuffer>(),
            28,
            "VAEncPackedHeaderParameterBuffer"
        );
        assert_eq!(size_of::<VAEncMiscParameterRateControl>(), 60, "VAEncMiscParameterRateControl");
        assert_eq!(size_of::<VAEncMiscParameterFrameRate>(), 24, "VAEncMiscParameterFrameRate");
        assert_eq!(size_of::<VAEncMiscParameterHRD>(), 24, "VAEncMiscParameterHRD");
        assert_eq!(
            size_of::<VAEncSequenceParameterBufferH264>(),
            1132,
            "VAEncSequenceParameterBufferH264"
        );
        assert_eq!(
            size_of::<VAEncPictureParameterBufferH264>(),
            648,
            "VAEncPictureParameterBufferH264"
        );
        assert_eq!(
            size_of::<VAEncSliceParameterBufferH264>(),
            3140,
            "VAEncSliceParameterBufferH264"
        );
        assert_eq!(size_of::<VAPictureHEVC>(), 28, "VAPictureHEVC");
        assert_eq!(size_of::<VAPictureParameterBufferHEVC>(), 604, "VAPictureParameterBufferHEVC");
        assert_eq!(size_of::<VASliceParameterBufferHEVC>(), 264, "VASliceParameterBufferHEVC");
        assert_eq!(size_of::<VAIQMatrixBufferHEVC>(), 1016, "VAIQMatrixBufferHEVC");
        assert_eq!(
            size_of::<VAEncSequenceParameterBufferHEVC>(),
            116,
            "VAEncSequenceParameterBufferHEVC"
        );
        assert_eq!(
            size_of::<VAEncPictureParameterBufferHEVC>(),
            576,
            "VAEncPictureParameterBufferHEVC"
        );
        assert_eq!(
            size_of::<VAEncSliceParameterBufferHEVC>(),
            1076,
            "VAEncSliceParameterBufferHEVC"
        );
        assert_eq!(
            size_of::<VAEncSequenceParameterBufferVP8>(),
            72,
            "VAEncSequenceParameterBufferVP8"
        );
        assert_eq!(
            size_of::<VAEncPictureParameterBufferVP8>(),
            60,
            "VAEncPictureParameterBufferVP8"
        );
        assert_eq!(size_of::<VAQMatrixBufferVP8>(), 36, "VAQMatrixBufferVP8");
    }

    #[test]
    fn vp9_decode_struct_sizes_match_libva() {
        use std::mem::offset_of;
        assert_eq!(size_of::<VADecPictureParameterBufferVP9>(), 92);
        assert_eq!(offset_of!(VADecPictureParameterBufferVP9, pic_fields), 36);
        assert_eq!(offset_of!(VADecPictureParameterBufferVP9, filter_level), 40);
        assert_eq!(offset_of!(VADecPictureParameterBufferVP9, first_partition_size), 46);
        assert_eq!(offset_of!(VADecPictureParameterBufferVP9, mb_segment_tree_probs), 48);
        assert_eq!(offset_of!(VADecPictureParameterBufferVP9, segment_pred_probs), 55);
        assert_eq!(offset_of!(VADecPictureParameterBufferVP9, profile), 58);
        assert_eq!(offset_of!(VADecPictureParameterBufferVP9, bit_depth), 59);
        assert_eq!(offset_of!(VADecPictureParameterBufferVP9, va_reserved), 60);
        assert_eq!(size_of::<VASegmentParameterVP9>(), 36);
        assert_eq!(offset_of!(VASegmentParameterVP9, filter_level), 2);
        assert_eq!(offset_of!(VASegmentParameterVP9, luma_ac_quant_scale), 10);
        assert_eq!(offset_of!(VASegmentParameterVP9, va_reserved), 20);
        assert_eq!(size_of::<VASliceParameterBufferVP9>(), 316);
        assert_eq!(offset_of!(VASliceParameterBufferVP9, seg_param), 12);
        assert_eq!(offset_of!(VASliceParameterBufferVP9, va_reserved), 300);
    }

    #[test]
    fn av1_decode_struct_layouts_match_libva() {
        use std::mem::offset_of;
        type Seg = VASegmentationStructAV1;
        type Fg = VAFilmGrainStructAV1;
        type Wm = VAWarpedMotionParamsAV1;
        type Pic = VADecPictureParameterBufferAV1;
        type Tile = VASliceParameterBufferAV1;
        assert_eq!(size_of::<Seg>(), 156);
        assert_eq!(offset_of!(Seg, feature_data), 4);
        assert_eq!(offset_of!(Seg, feature_mask), 132);
        assert_eq!(offset_of!(Seg, va_reserved), 140);
        assert_eq!(size_of::<Fg>(), 176);
        assert_eq!(offset_of!(Fg, grain_seed), 4);
        assert_eq!(offset_of!(Fg, num_y_points), 6);
        assert_eq!(offset_of!(Fg, point_y_value), 7);
        assert_eq!(offset_of!(Fg, num_cb_points), 35);
        assert_eq!(offset_of!(Fg, num_cr_points), 56);
        assert_eq!(offset_of!(Fg, ar_coeffs_y), 77);
        assert_eq!(offset_of!(Fg, ar_coeffs_cb), 101);
        assert_eq!(offset_of!(Fg, ar_coeffs_cr), 126);
        assert_eq!(offset_of!(Fg, cb_mult), 151);
        assert_eq!(offset_of!(Fg, cb_offset), 154);
        assert_eq!(offset_of!(Fg, cr_mult), 156);
        assert_eq!(offset_of!(Fg, cr_offset), 158);
        assert_eq!(offset_of!(Fg, va_reserved), 160);
        assert_eq!(size_of::<Wm>(), 56);
        assert_eq!(offset_of!(Wm, wmmat), 4);
        assert_eq!(offset_of!(Wm, invalid), 36);
        assert_eq!(offset_of!(Wm, va_reserved), 40);
        assert_eq!(size_of::<Pic>(), 1160);
        assert_eq!(align_of::<Pic>(), 8);
        assert_eq!(offset_of!(Pic, seq_info_fields), 4);
        assert_eq!(offset_of!(Pic, current_frame), 8);
        assert_eq!(offset_of!(Pic, current_display_picture), 12);
        assert_eq!(offset_of!(Pic, anchor_frames_num), 16);
        assert_eq!(offset_of!(Pic, anchor_frames_list), 24);
        assert_eq!(offset_of!(Pic, frame_width_minus1), 32);
        assert_eq!(offset_of!(Pic, output_frame_width_in_tiles_minus_1), 36);
        assert_eq!(offset_of!(Pic, ref_frame_map), 40);
        assert_eq!(offset_of!(Pic, ref_frame_idx), 72);
        assert_eq!(offset_of!(Pic, primary_ref_frame), 79);
        assert_eq!(offset_of!(Pic, order_hint), 80);
        assert_eq!(offset_of!(Pic, seg_info), 84);
        assert_eq!(offset_of!(Pic, film_grain_info), 240);
        assert_eq!(offset_of!(Pic, tile_cols), 416);
        assert_eq!(offset_of!(Pic, width_in_sbs_minus_1), 418);
        assert_eq!(offset_of!(Pic, height_in_sbs_minus_1), 544);
        assert_eq!(offset_of!(Pic, tile_count_minus_1), 670);
        assert_eq!(offset_of!(Pic, context_update_tile_id), 672);
        assert_eq!(offset_of!(Pic, pic_info_fields), 676);
        assert_eq!(offset_of!(Pic, superres_scale_denominator), 680);
        assert_eq!(offset_of!(Pic, interp_filter), 681);
        assert_eq!(offset_of!(Pic, filter_level), 682);
        assert_eq!(offset_of!(Pic, filter_level_u), 684);
        assert_eq!(offset_of!(Pic, filter_level_v), 685);
        assert_eq!(offset_of!(Pic, loop_filter_info_fields), 686);
        assert_eq!(offset_of!(Pic, ref_deltas), 687);
        assert_eq!(offset_of!(Pic, mode_deltas), 695);
        assert_eq!(offset_of!(Pic, base_qindex), 697);
        assert_eq!(offset_of!(Pic, y_dc_delta_q), 698);
        assert_eq!(offset_of!(Pic, v_ac_delta_q), 702);
        assert_eq!(offset_of!(Pic, qmatrix_fields), 704);
        assert_eq!(offset_of!(Pic, mode_control_fields), 708);
        assert_eq!(offset_of!(Pic, cdef_damping_minus_3), 712);
        assert_eq!(offset_of!(Pic, cdef_bits), 713);
        assert_eq!(offset_of!(Pic, cdef_y_strengths), 714);
        assert_eq!(offset_of!(Pic, cdef_uv_strengths), 722);
        assert_eq!(offset_of!(Pic, loop_restoration_fields), 730);
        assert_eq!(offset_of!(Pic, wm), 732);
        assert_eq!(offset_of!(Pic, va_reserved), 1124);
        assert_eq!(size_of::<Tile>(), 40);
        assert_eq!(offset_of!(Tile, tile_row), 12);
        assert_eq!(offset_of!(Tile, tile_column), 14);
        assert_eq!(offset_of!(Tile, tg_start), 16);
        assert_eq!(offset_of!(Tile, tg_end), 18);
        assert_eq!(offset_of!(Tile, anchor_frame_idx), 20);
        assert_eq!(offset_of!(Tile, tile_idx_in_tile_list), 22);
        assert_eq!(offset_of!(Tile, va_reserved), 24);
        assert_eq!(AV1_SEQ_FILM_GRAIN_PARAMS_PRESENT, 1 << 15);
        assert_eq!(AV1_PIC_LARGE_SCALE_TILE, 1 << 16);
        assert_eq!(AV1_LF_MODE_REF_DELTA_UPDATE, 1 << 4);
        assert_eq!(AV1_QM_V_SHIFT, 9);
        assert_eq!(AV1_MODE_SKIP_MODE_PRESENT, 1 << 11);
        assert_eq!(AV1_LR_UV_SHIFT, 1 << 8);
        assert_eq!(AV1_SEG_UPDATE_DATA, 1 << 3);
        assert_eq!(AV1_FG_CLIP_TO_RESTRICTED_RANGE, 1 << 11);
        assert_eq!(VAProfileAV1Profile0, 32);
    }

    #[test]
    fn drm_prime_export_struct_sizes_match_va_drmcommon() {
        assert_eq!(
            size_of::<VADRMPRIMESurfaceDescriptorObject>(),
            16,
            "VADRMPRIMESurfaceDescriptorObject"
        );
        assert_eq!(align_of::<VADRMPRIMESurfaceDescriptorObject>(), 8);
        assert_eq!(
            size_of::<VADRMPRIMESurfaceDescriptorLayer>(),
            56,
            "VADRMPRIMESurfaceDescriptorLayer"
        );
        assert_eq!(align_of::<VADRMPRIMESurfaceDescriptorLayer>(), 4);
        assert_eq!(size_of::<VADRMPRIMESurfaceDescriptor>(), 312, "VADRMPRIMESurfaceDescriptor");
        assert_eq!(align_of::<VADRMPRIMESurfaceDescriptor>(), 8);
    }

    #[test]
    fn video_proc_struct_layouts_match_libva() {
        use std::mem::offset_of;
        assert_eq!(size_of::<VARectangle>(), 8);
        assert_eq!(size_of::<VAProcColorProperties>(), 8);
        assert_eq!(size_of::<VAProcPipelineParameterBuffer>(), 224);
        assert_eq!(offset_of!(VAProcPipelineParameterBuffer, surface_region), 8);
        assert_eq!(offset_of!(VAProcPipelineParameterBuffer, output_region), 24);
        assert_eq!(offset_of!(VAProcPipelineParameterBuffer, filter_flags), 44);
        assert_eq!(offset_of!(VAProcPipelineParameterBuffer, num_backward_references), 88);
        assert_eq!(offset_of!(VAProcPipelineParameterBuffer, rotation_state), 92);
        assert_eq!(offset_of!(VAProcPipelineParameterBuffer, blend_state), 96);
        assert_eq!(offset_of!(VAProcPipelineParameterBuffer, mirror_state), 104);
        assert_eq!(offset_of!(VAProcPipelineParameterBuffer, input_surface_flag), 124);
        assert_eq!(offset_of!(VAProcPipelineParameterBuffer, input_color_properties), 132);
        assert_eq!(offset_of!(VAProcPipelineParameterBuffer, output_color_properties), 140);
        assert_eq!(offset_of!(VAProcPipelineParameterBuffer, processing_mode), 148);
        assert_eq!(offset_of!(VAProcPipelineParameterBuffer, output_hdr_metadata), 152);
        assert_eq!(offset_of!(VAProcPipelineParameterBuffer, va_reserved), 160);
        assert_eq!(size_of::<VAHdrMetaDataHDR10>(), 60);
        assert_eq!(offset_of!(VAHdrMetaDataHDR10, max_display_mastering_luminance), 16);
        assert_eq!(offset_of!(VAHdrMetaDataHDR10, max_content_light_level), 24);
        assert_eq!(size_of::<VAHdrMetaData>(), 40);
        assert_eq!(offset_of!(VAHdrMetaData, metadata), 8);
        assert_eq!(offset_of!(VAHdrMetaData, metadata_size), 16);
        assert_eq!(size_of::<VAProcFilterParameterBufferHDRToneMapping>(), 48);
        assert_eq!(offset_of!(VAProcFilterParameterBufferHDRToneMapping, data), 8);
        assert_eq!(size_of::<VAProcFilterCapHighDynamicRange>(), 40);
        assert_eq!(size_of::<VAProcPipelineCaps>(), 176);
        assert_eq!(offset_of!(VAProcPipelineCaps, input_color_standards), 16);
        assert_eq!(offset_of!(VAProcPipelineCaps, output_color_standards), 32);
        assert_eq!(offset_of!(VAProcPipelineCaps, rotation_flags), 44);
        assert_eq!(offset_of!(VAProcPipelineCaps, num_input_pixel_formats), 60);
        assert_eq!(offset_of!(VAProcPipelineCaps, input_pixel_format), 64);
        assert_eq!(offset_of!(VAProcPipelineCaps, output_pixel_format), 80);
        assert_eq!(offset_of!(VAProcPipelineCaps, max_input_width), 88);
        assert_eq!(offset_of!(VAProcPipelineCaps, min_output_height), 116);
        assert_eq!(offset_of!(VAProcPipelineCaps, va_reserved), 120);
    }

    #[test]
    fn vp9_encode_struct_layouts_match_libva() {
        use std::mem::offset_of;
        type Pic = VAEncPictureParameterBufferVP9;
        assert_eq!(size_of::<VAEncSequenceParameterBufferVP9>(), 44);
        assert_eq!(offset_of!(VAEncSequenceParameterBufferVP9, intra_period), 24);
        assert_eq!(size_of::<Pic>(), 132);
        assert_eq!(offset_of!(Pic, reconstructed_frame), 16);
        assert_eq!(offset_of!(Pic, reference_frames), 20);
        assert_eq!(offset_of!(Pic, coded_buf), 52);
        assert_eq!(offset_of!(Pic, ref_flags), 56);
        assert_eq!(offset_of!(Pic, pic_flags), 60);
        assert_eq!(offset_of!(Pic, refresh_frame_flags), 64);
        assert_eq!(offset_of!(Pic, luma_ac_qindex), 65);
        assert_eq!(offset_of!(Pic, filter_level), 69);
        assert_eq!(offset_of!(Pic, ref_lf_delta), 71);
        assert_eq!(offset_of!(Pic, mode_lf_delta), 75);
        assert_eq!(offset_of!(Pic, bit_offset_ref_lf_delta), 78);
        assert_eq!(offset_of!(Pic, bit_size_segmentation), 90);
        assert_eq!(offset_of!(Pic, log2_tile_rows), 92);
        assert_eq!(offset_of!(Pic, skip_frames_size), 96);
        assert_eq!(offset_of!(Pic, seg_id_block_size), 100);
        assert_eq!(offset_of!(Pic, va_reserved), 104);
    }
}
