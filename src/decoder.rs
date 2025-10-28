use std::{
    mem::MaybeUninit,
    os::raw::{c_int, c_void},
    ptr, slice,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use ctor::ctor;
use media_codec::{
    codec::{Codec, CodecBuilder, CodecID},
    decoder::{register_decoder, Decoder, DecoderBuilder, VideoDecoder, VideoDecoderParameters},
    packet::Packet,
    CodecInformation, CodecParameters,
};
use media_core::{
    buffer::{Buffer, BufferPool},
    error::Error,
    frame::{Frame, SharedFrame},
    frame_pool::{FrameCreator, FramePool},
    unsupported_error,
    variant::Variant,
    video::{ColorMatrix, ColorRange, PixelFormat, VideoFrameDescriptor},
    FrameDescriptor, Result,
};
use smallvec::SmallVec;

use crate::{
    vpx_error_string,
    vpx_sys::{
        self, vpx_codec_ctx_t, vpx_codec_err_t::VPX_CODEC_OK, vpx_codec_frame_buffer_t, vpx_codec_iter_t, vpx_color_range, vpx_color_space,
        vpx_image_t, vpx_img_fmt, VPX_DECODER_ABI_VERSION,
    },
};

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

const DEFAULT_MAX_VIDEO_PLANES: usize = 4;

type BufferPlaneVec = SmallVec<[(usize, u32); DEFAULT_MAX_VIDEO_PLANES]>;

struct VPXImage(vpx_image_t);

impl VPXImage {
    fn has_frame_buffer(&self) -> bool {
        !self.0.fb_priv.is_null()
    }

    fn descriptor(&self) -> Result<VideoFrameDescriptor> {
        let img = &self.0;
        let frame_width = img.d_w;
        let frame_height = img.d_h;
        let pix_fmt = img.fmt;
        let depth = img.bit_depth;

        let pixel_format = vpx_img_fmt_to_pixel_format(pix_fmt, depth).ok_or_else(|| unsupported_error!(pix_fmt))?;

        let mut desc = VideoFrameDescriptor::try_new(pixel_format, frame_width, frame_height)?;
        desc.color_range = vpx_color_range_to_color_range(img.range);
        desc.color_matrix = vpx_color_space_to_color_matrix(img.cs);

        Ok(desc)
    }

    fn convert_to_frame(&self) -> Result<Frame<'_>> {
        let img = &self.0;
        let desc = self.descriptor()?;
        let planes_num = desc.format.components() as usize;
        let mut buffers = SmallVec::<[(&[u8], u32); DEFAULT_MAX_VIDEO_PLANES]>::with_capacity(planes_num);

        for plane in 0..planes_num {
            let height = desc.format.calc_plane_height(plane, desc.height.get()) as usize;
            let stride = img.stride[plane] as usize;
            let buffer = unsafe { slice::from_raw_parts(img.planes[plane], stride * height) };
            buffers.push((buffer, stride as u32));
        }

        let frame = Frame::video_creator().create_from_buffers_with_descriptor(desc, &buffers)?;

        Ok(frame)
    }

    fn convert_to_buffer(&self) -> Result<(Arc<Buffer>, BufferPlaneVec, VideoFrameDescriptor)> {
        let img = &self.0;
        let desc = self.descriptor()?;
        let planes_num = desc.format.components() as usize;
        let buffer = unsafe { Arc::from_raw(img.fb_priv as *const Buffer) };

        let mut buffers = SmallVec::with_capacity(planes_num);

        for plane in 0..planes_num {
            let offset = img.planes[plane] as usize - buffer.data().as_ptr() as usize;

            if offset >= buffer.len() {
                let _ = Arc::into_raw(buffer);
                return Err(Error::Invalid("invalid frame buffer offset".to_string()));
            }

            let stride = img.stride[plane] as usize;
            buffers.push((offset, stride as u32));
        }

        let buffer_clone = buffer.clone();
        let _ = Arc::into_raw(buffer);

        Ok((buffer_clone, buffers, desc))
    }
}

struct EmptyFrameCreator;

impl FrameCreator for EmptyFrameCreator {
    fn create_frame(&self, desc: FrameDescriptor) -> Result<Frame<'static>> {
        Frame::video_creator().create_empty_with_descriptor(desc.try_into()?)
    }
}

pub struct VPXDecoder {
    id: CodecID,
    name: &'static str,
    ctx: vpx_codec_ctx_t,
    iter: vpx_codec_iter_t,
    buffer_pool_ptr: *const BufferPool,
    frame_pool_initialized: AtomicBool,
}

unsafe impl Send for VPXDecoder {}
unsafe impl Sync for VPXDecoder {}

impl Codec<VideoDecoder> for VPXDecoder {
    fn configure(&mut self, _params: Option<&CodecParameters>, _options: Option<&Variant>) -> Result<()> {
        Ok(())
    }

    fn set_option(&mut self, _name: &str, _value: &Variant) -> Result<()> {
        Ok(())
    }
}

impl Decoder<VideoDecoder> for VPXDecoder {
    fn send_packet(&mut self, _config: &VideoDecoder, _pool: Option<&Arc<FramePool<Frame<'static>>>>, packet: Packet) -> Result<()> {
        let packet_data = packet.data();
        let ret = unsafe { vpx_sys::vpx_codec_decode(&mut self.ctx, packet_data.as_ptr(), packet_data.len() as u32, ptr::null_mut(), 0) };

        self.iter = ptr::null_mut();

        if ret != VPX_CODEC_OK {
            return Err(Error::Invalid(vpx_error_string(ret)));
        }

        Ok(())
    }

    fn receive_frame(&mut self, _config: &VideoDecoder, pool: Option<&Arc<FramePool<Frame<'static>>>>) -> Result<SharedFrame<Frame<'static>>> {
        let img = &self.get_image()?;

        let pool = if let Some(pool) = pool {
            pool
        } else {
            if !img.has_frame_buffer() {
                return img.convert_to_frame().map(SharedFrame::<Frame<'static>>::new);
            }

            let (buffer, buffer_planes, desc) = img.convert_to_buffer()?;
            let frame = Frame::video_creator().create_from_shared_buffer_with_descriptor(desc, buffer, &buffer_planes)?;

            return Ok(SharedFrame::<Frame<'static>>::new(frame));
        };

        if !img.has_frame_buffer() {
            let desc = img.descriptor()?;

            if !self.frame_pool_initialized.load(Ordering::Relaxed) {
                pool.configure(Some(desc.clone().into()), None);
                self.frame_pool_initialized.store(true, Ordering::Relaxed);
            }

            let frame = img.convert_to_frame()?;
            let mut pooled_frame = pool.get_frame_with_descriptor(desc.into())?;
            frame.convert_to(pooled_frame.write().unwrap())?;

            Ok(pooled_frame)
        } else {
            let (buffer, buffer_planes, desc) = img.convert_to_buffer()?;

            if !self.frame_pool_initialized.load(Ordering::Relaxed) {
                pool.configure(Some(desc.clone().into()), Some(Box::new(EmptyFrameCreator)));
                self.frame_pool_initialized.store(true, Ordering::Relaxed);
            }

            let mut pooled_frame = pool.get_frame_with_descriptor(desc.clone().into())?;
            pooled_frame.write().unwrap().attach_video_shared_buffer_with_descriptor(desc, buffer, &buffer_planes)?;

            Ok(pooled_frame)
        }
    }

    fn receive_frame_borrowed(&mut self, _config: &VideoDecoder) -> Result<Frame<'_>> {
        Err(unsupported_error!("borrowed frame"))
    }

    fn flush(&mut self, _config: &VideoDecoder) -> Result<()> {
        let ret = unsafe { vpx_sys::vpx_codec_decode(&mut self.ctx, ptr::null(), 0, ptr::null_mut(), 0) };

        self.iter = ptr::null_mut();

        if ret != VPX_CODEC_OK {
            return Err(Error::Invalid(vpx_error_string(ret)));
        }

        Ok(())
    }
}

unsafe extern "C" fn get_frame_buffer(priv_: *mut c_void, min_size: usize, fb: *mut vpx_codec_frame_buffer_t) -> c_int {
    let pool = Arc::from_raw(priv_ as *const BufferPool);

    if pool.get_buffer_capacity() < min_size {
        pool.set_buffer_capacity(min_size);
    }

    let buffer = pool.get_buffer();

    (*fb).data = buffer.data().as_ptr() as *mut u8;
    (*fb).size = buffer.len();
    (*fb).priv_ = Arc::into_raw(buffer) as *mut c_void;

    let _ = Arc::into_raw(pool);

    0
}

unsafe extern "C" fn release_frame_buffer(_priv_: *mut c_void, fb: *mut vpx_codec_frame_buffer_t) -> c_int {
    if !(*fb).priv_.is_null() {
        let buffer = Arc::from_raw((*fb).priv_ as *const Buffer);
        drop(buffer);
    }

    0
}

impl VPXDecoder {
    pub fn new(id: CodecID, _params: &VideoDecoderParameters, _options: Option<&Variant>) -> Result<Self> {
        let (iface, name) = match id {
            CodecID::VP8 => (unsafe { vpx_sys::vpx_codec_vp8_dx() }, VP8_CODEC_NAME),
            CodecID::VP9 => (unsafe { vpx_sys::vpx_codec_vp9_dx() }, VP9_CODEC_NAME),
            _ => return Err(unsupported_error!(id)),
        };

        let mut ctx = MaybeUninit::uninit();
        let cfg = MaybeUninit::zeroed();
        let ver = VPX_DECODER_ABI_VERSION as i32;
        let ret = unsafe { vpx_sys::vpx_codec_dec_init_ver(ctx.as_mut_ptr(), iface, cfg.as_ptr(), 0, ver) };

        if ret != VPX_CODEC_OK {
            return Err(Error::Invalid(vpx_error_string(ret)));
        }

        let pool = BufferPool::new(0);
        let pool_ptr = Arc::into_raw(pool);

        if id == CodecID::VP9 {
            unsafe {
                vpx_sys::vpx_codec_set_frame_buffer_functions(
                    ctx.as_mut_ptr(),
                    Some(get_frame_buffer),
                    Some(release_frame_buffer),
                    pool_ptr as *mut c_void,
                );
            }
        }

        Ok(Self {
            id,
            name,
            ctx: unsafe { ctx.assume_init() },
            iter: ptr::null_mut(),
            buffer_pool_ptr: pool_ptr,
            frame_pool_initialized: AtomicBool::new(false),
        })
    }

    fn get_image(&mut self) -> Result<VPXImage> {
        let img = unsafe { vpx_sys::vpx_codec_get_frame(&mut self.ctx as *const _ as *mut _, &mut self.iter) };
        if img.is_null() {
            return Err(Error::Again("no frame available".to_string()));
        }

        let img = unsafe { *img };

        Ok(VPXImage(img))
    }
}

impl Drop for VPXDecoder {
    fn drop(&mut self) {
        unsafe {
            vpx_sys::vpx_codec_destroy(&mut self.ctx);

            if !self.buffer_pool_ptr.is_null() {
                let pool = Arc::from_raw(self.buffer_pool_ptr);
                drop(pool);
            }
        };
    }
}

pub struct VPXDecoderBuilder {
    id: CodecID,
    name: &'static str,
}

impl DecoderBuilder<VideoDecoder> for VPXDecoderBuilder {
    fn new_decoder(&self, codec_id: CodecID, params: &CodecParameters, options: Option<&Variant>) -> Result<Box<dyn Decoder<VideoDecoder>>> {
        Ok(Box::new(VPXDecoder::new(codec_id, &params.try_into()?, options)?))
    }
}

impl CodecBuilder<VideoDecoder> for VPXDecoderBuilder {
    fn id(&self) -> CodecID {
        self.id
    }

    fn name(&self) -> &'static str {
        self.name
    }
}

impl CodecInformation for VPXDecoder {
    fn id(&self) -> CodecID {
        self.id
    }

    fn name(&self) -> &'static str {
        self.name
    }
}

const VP8_CODEC_NAME: &str = "vp8-dec";
const VP9_CODEC_NAME: &str = "vp9-dec";

const VP8_DECODER_BUILDER: VPXDecoderBuilder = VPXDecoderBuilder {
    id: CodecID::VP8,
    name: VP8_CODEC_NAME,
};

const VP9_DECODER_BUILDER: VPXDecoderBuilder = VPXDecoderBuilder {
    id: CodecID::VP9,
    name: VP9_CODEC_NAME,
};

#[ctor]
pub fn initialize() {
    register_decoder(Arc::new(VP8_DECODER_BUILDER), false);
    register_decoder(Arc::new(VP9_DECODER_BUILDER), false);
}
