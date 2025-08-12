use std::{ffi::CStr, mem::MaybeUninit, ptr, slice, sync::Arc};

use ctor::ctor;
use media_codec::{
    codec::{Codec, CodecBuilder, CodecID, CodecParameters},
    decoder::{register_decoder, Decoder, DecoderBuilder},
    packet::Packet,
};
use media_codec_vpx_sys::vpx_codec_iter_t;
use media_core::{
    error::Error,
    frame::Frame,
    unsupported_error,
    variant::Variant,
    video::{ColorMatrix, ColorRange, PixelFormat, VideoFrameDescriptor},
    Result,
};

use crate::vpx_sys::{self, vpx_codec_ctx_t, vpx_codec_err_t::VPX_CODEC_OK, vpx_color_range, vpx_color_space, vpx_img_fmt, VPX_DECODER_ABI_VERSION};

fn vpx_img_fmt_to_pixel_format(fmt: vpx_img_fmt, depth: u32) -> Option<PixelFormat> {
    use vpx_img_fmt::*;

    match (fmt, depth) {
        (VPX_IMG_FMT_YV12, _) => Some(PixelFormat::YV12),
        (VPX_IMG_FMT_I420, _) => Some(PixelFormat::I420),
        (VPX_IMG_FMT_I422, _) => Some(PixelFormat::I422),
        (VPX_IMG_FMT_I444, _) => Some(PixelFormat::I444),
        (VPX_IMG_FMT_NV12, _) => Some(PixelFormat::NV12),
        (VPX_IMG_FMT_I42016, 10) => Some(PixelFormat::I010),
        (VPX_IMG_FMT_I42016, 12) => Some(PixelFormat::I012),
        (VPX_IMG_FMT_I42216, 10) => Some(PixelFormat::I210),
        (VPX_IMG_FMT_I42216, 12) => Some(PixelFormat::I212),
        (VPX_IMG_FMT_I44416, 10) => Some(PixelFormat::I410),
        (VPX_IMG_FMT_I44416, 12) => Some(PixelFormat::I412),
        _ => None,
    }
}

fn vpx_color_range_to_color_range(color_range: vpx_color_range) -> ColorRange {
    use vpx_color_range::*;

    match color_range {
        VPX_CR_STUDIO_RANGE => ColorRange::Video,
        VPX_CR_FULL_RANGE => ColorRange::Full,
    }
}

fn vpx_color_space_to_color_matrix(color_space: vpx_color_space) -> ColorMatrix {
    use vpx_color_space::*;

    match color_space {
        VPX_CS_UNKNOWN => ColorMatrix::Unspecified,
        VPX_CS_BT_601 => ColorMatrix::BT470BG,
        VPX_CS_BT_709 => ColorMatrix::BT709,
        VPX_CS_SMPTE_170 => ColorMatrix::SMPTE170M,
        VPX_CS_SMPTE_240 => ColorMatrix::SMPTE240M,
        VPX_CS_BT_2020 => ColorMatrix::BT2020NCL,
        VPX_CS_RESERVED => ColorMatrix::Reserved,
        VPX_CS_SRGB => ColorMatrix::Identity,
    }
}

pub struct VPXDecoder {
    ctx: vpx_codec_ctx_t,
    iter: vpx_codec_iter_t,
}

unsafe impl Send for VPXDecoder {}
unsafe impl Sync for VPXDecoder {}

impl Codec for VPXDecoder {
    fn configure(&mut self, _parameters: Option<&CodecParameters>, _options: Option<&Variant>) -> Result<()> {
        Ok(())
    }

    fn set_option(&mut self, _name: &str, _value: &Variant) -> Result<()> {
        Ok(())
    }
}

impl Decoder for VPXDecoder {
    fn send_packet(&mut self, _parameters: Option<&CodecParameters>, packet: &Packet) -> Result<()> {
        let ret = unsafe { vpx_sys::vpx_codec_decode(&mut self.ctx, packet.data.as_ptr(), packet.data.len() as u32, ptr::null_mut(), 0) };

        self.iter = ptr::null_mut();

        if ret != VPX_CODEC_OK {
            let c_str = unsafe { vpx_sys::vpx_codec_err_to_string(ret) };
            let msg = unsafe { CStr::from_ptr(c_str).to_string_lossy().into_owned() };
            return Err(Error::Invalid(msg));
        }

        Ok(())
    }

    fn receive_frame(&mut self, _parameters: Option<&CodecParameters>) -> Result<Frame<'_>> {
        let img = unsafe { vpx_sys::vpx_codec_get_frame(&mut self.ctx, &mut self.iter) };
        if img.is_null() {
            return Err(Error::Again("no frame available".to_string()));
        }

        let img_ref = unsafe { &*img };
        let frame_width = img_ref.d_w;
        let frame_height = img_ref.d_h;
        let pix_fmt = img_ref.fmt;
        let depth = img_ref.bit_depth;

        let pixel_format = vpx_img_fmt_to_pixel_format(pix_fmt, depth).ok_or_else(|| unsupported_error!(pix_fmt))?;

        let planes = pixel_format.components() as usize;
        let mut buffers = Vec::with_capacity(planes);

        for plane in 0..planes {
            let height = pixel_format.calc_plane_height(plane, frame_height) as usize;
            let stride = img_ref.stride[plane] as usize;
            let buffer = unsafe { slice::from_raw_parts(img_ref.planes[plane], stride * height) };
            buffers.push((buffer, stride as u32));
        }

        let mut desc = VideoFrameDescriptor::try_new(pixel_format, frame_width, frame_height)?;
        desc.color_range = vpx_color_range_to_color_range(img_ref.range);
        desc.color_matrix = vpx_color_space_to_color_matrix(img_ref.cs);

        let frame = Frame::video_creator().create_from_buffers_with_descriptor(desc, &buffers)?;

        Ok(frame)
    }
}

impl Drop for VPXDecoder {
    fn drop(&mut self) {
        unsafe { vpx_sys::vpx_codec_destroy(&mut self.ctx) };
    }
}

impl VPXDecoder {
    pub fn new(codec_id: CodecID, _parameters: Option<CodecParameters>, _options: Option<Variant>) -> Result<Self> {
        let iface = match codec_id {
            CodecID::VP8 => unsafe { vpx_sys::vpx_codec_vp8_dx() },
            CodecID::VP9 => unsafe { vpx_sys::vpx_codec_vp9_dx() },
            _ => return Err(unsupported_error!(codec_id)),
        };

        let mut ctx = MaybeUninit::uninit();
        let cfg = MaybeUninit::zeroed();
        let ver = VPX_DECODER_ABI_VERSION as i32;
        let ret = unsafe { vpx_sys::vpx_codec_dec_init_ver(ctx.as_mut_ptr(), iface, cfg.as_ptr(), 0, ver) };

        if ret != VPX_CODEC_OK {
            return Err(Error::Invalid(unsafe { CStr::from_ptr(vpx_sys::vpx_codec_err_to_string(ret)).to_string_lossy().into_owned() }));
        }

        Ok(Self {
            ctx: unsafe { ctx.assume_init() },
            iter: ptr::null_mut(),
        })
    }
}

pub struct VPXDecoderBuilder {
    codec_id: CodecID,
    name: &'static str,
}

impl DecoderBuilder for VPXDecoderBuilder {
    fn new_decoder(&self, codec_id: CodecID, parameters: Option<CodecParameters>, options: Option<Variant>) -> Result<Box<dyn Decoder>> {
        Ok(Box::new(VPXDecoder::new(codec_id, parameters, options)?))
    }
}

impl CodecBuilder for VPXDecoderBuilder {
    fn id(&self) -> CodecID {
        self.codec_id
    }

    fn name(&self) -> &'static str {
        self.name
    }
}

const VP8_DECODER_NAME: &str = "vp8-dec";
const VP9_DECODER_NAME: &str = "vp9-dec";

const VP8_DECODER_BUILDER: VPXDecoderBuilder = VPXDecoderBuilder {
    codec_id: CodecID::VP8,
    name: VP8_DECODER_NAME,
};

const VP9_DECODER_BUILDER: VPXDecoderBuilder = VPXDecoderBuilder {
    codec_id: CodecID::VP9,
    name: VP9_DECODER_NAME,
};

#[ctor]
fn initialize() {
    register_decoder(Arc::new(VP8_DECODER_BUILDER), false);
    register_decoder(Arc::new(VP9_DECODER_BUILDER), false);
}
