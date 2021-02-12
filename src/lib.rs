#![cfg_attr(not(feature = "std"), no_std)]

use core::{mem::MaybeUninit, ptr};
use libc::c_int;

/// Raw minimp3 bindings if needed,
/// although if there's a desired API feature please make an issue/PR.
#[allow(clippy::all, non_camel_case_types)]
#[path = "bindings.rs"]
pub mod ffi;

/// Conditional type used to represent one PCM sample in output data.
///
/// Normally a signed 16-bit integer (i16), but if the *"float"* feature is enabled,
/// it's a 32-bit single precision float (f32).
#[cfg(not(feature = "float"))]
pub type Sample = i16;
#[cfg(feature = "float")]
pub type Sample = f32;

/// Maximum amount of samples that can be yielded per frame.
pub const MAX_SAMPLES_PER_FRAME: usize = ffi::MINIMP3_MAX_SAMPLES_PER_FRAME as usize;

/// Audio or miscellaneous data in a frame.
pub enum Frame<'src, 'pcm> {
    /// PCM Sample Data
    Audio(Samples<'src, 'pcm>),

    /// Unknown Data
    Unknown {
        /// Total bytes consumed from the start of the input data.
        bytes_consumed: usize,
        /// Source bytes of the frame, including the header, excluding skipped (potential) garbage data.
        source: &'src [u8],
    },
}

/// Primitive decoder for parsing or decoding MPEG Audio data.
///
/// This is quite low-level and stores no state.
/// If you can load the entire file at once, [`DecoderStream`] is more convenient.
pub struct Decoder(MaybeUninit<ffi::mp3dec_t>);

/// Static buffer for holding PCM data, used alongside a [`Decoder`].
#[repr(transparent)]
pub struct DecoderBuf([Sample; MAX_SAMPLES_PER_FRAME]);

/// High-level streaming iterator for parsing or decoding MPEG Audio data.
///
/// Potentially faster than a [`Decoder`] if planning to seek/decode entire data.
pub struct DecoderStream<'src> {
    decoder: MaybeUninit<ffi::mp3dec_t>,
    decoder_buf: DecoderBuf,
    frame_recv: MaybeUninit<ffi::mp3dec_frame_info_t>,
    peek_cache_len: Option<usize>,
    source: &'src [u8],
    source_copy: &'src [u8],
}

#[cfg(feature = "std")]
pub struct DecoderStreamOwned {
    _data: Box<[u8]>,
    inner: DecoderStream<'static>,
}

/// PCM frame data yielded by a decoder.
///
/// Note that if a `peek`ing function was used, [`samples`](Self::samples) will be empty.
pub struct Samples<'src, 'pcm> {
    /// Bitrate of the source frame in kb/s.
    pub bitrate: u32,
    /// Number of channels in this frame.
    pub channels: u32,
    /// MPEG layer of this frame.
    pub mpeg_layer: u32,
    /// Sample rate of this frame in Hz.
    pub sample_rate: u32,

    /// Total bytes consumed from the start of the input data.
    pub bytes_consumed: usize,
    /// Source bytes of the frame, including the header,
    /// excluding skipped (potential) garbage data.
    pub source: &'src [u8],
    /// Reference to the samples in this frame,
    /// contained in the output buffer.
    /// Empty if using [`peek`](Decoder::peek).
    pub samples: &'pcm [Sample],
    /// Total sample count if using a `peek`ing function,
    /// since [`samples`](Samples::samples) would be empty.
    pub sample_count: usize,
}

/// Unit error type representing insufficient data in the input slice.
pub struct InsufficientData;

impl Decoder {
    /// Constructs a new `Decoder` for processing MPEG audio data.
    pub fn new() -> Self {
        let mut decoder = MaybeUninit::<ffi::mp3dec_t>::uninit();
        unsafe {
            ffi::mp3dec_init(decoder.as_mut_ptr());
        }
        Self(decoder)
    }

    /// Reads a frame without actually decoding it.
    ///
    /// This means that the samples will always be empty in a frame containing PCM data,
    /// therefore [`sample_count`](Samples::sample_count) should be used instead to inspect the length.
    #[inline(always)]
    pub fn peek<'src>(
        &mut self,
        data: &'src [u8],
    ) -> Result<Frame<'src, 'static>, InsufficientData> {
        self.dec(data, None)
    }

    /// Reads the next frame, skipping over garbage, returning a [`Frame`] if successful.
    #[inline(always)]
    pub fn next<'src, 'pcm>(
        &mut self,
        data: &'src [u8],
        buf: &'pcm mut DecoderBuf,
    ) -> Result<Frame<'src, 'pcm>, InsufficientData> {
        self.dec(data, Some(buf))
    }

    fn dec<'src, 'pcm>(
        &mut self,
        data: &'src [u8],
        buf: Option<&'pcm mut DecoderBuf>,
    ) -> Result<Frame<'src, 'pcm>, InsufficientData> {
        unsafe {
            let mut frame_recv = MaybeUninit::uninit();
            let data_len = data_len_safe(data.len());
            let pcm_ptr = buf
                .map(|x| x.0.as_ptr())
                .unwrap_or(ptr::null_mut());
            let samples = ffi::mp3dec_decode_frame(
                self.0.as_mut_ptr(),
                data.as_ptr(),
                data_len,
                pcm_ptr as *mut Sample,
                frame_recv.as_mut_ptr(),
            );
            let frame_recv = &*frame_recv.as_ptr();
            translate_response(frame_recv, samples, data, |pcm_points| {
                if !pcm_ptr.is_null() {
                    (&*(pcm_ptr as *const [Sample; MAX_SAMPLES_PER_FRAME])).get_unchecked(..pcm_points)
                } else {
                    &[]
                }
            })
        }
    }
}

impl DecoderBuf {
    /// Constructs a new `DecoderBuf`.
    pub const fn new() -> Self {
        Self([0 as Sample; MAX_SAMPLES_PER_FRAME])
    }
}

impl<'src> DecoderStream<'src> {
    /// Constructs a new [`DecoderStream`] over the given MPEG audio bytes.
    pub fn new(source: &'src [u8]) -> Self {
        Self {
            decoder: unsafe {
                let mut decoder = MaybeUninit::<ffi::mp3dec_t>::uninit();
                ffi::mp3dec_init(decoder.as_mut_ptr());
                decoder
            },
            decoder_buf: unsafe { MaybeUninit::uninit().assume_init() },
            frame_recv: MaybeUninit::uninit(),
            peek_cache_len: None,
            source,
            source_copy: source,
        }
    }

    /// Reads the next frame, skipping over garbage, returning a [`Frame`] if successful.
    pub fn next<'pcm>(&'pcm mut self) -> Result<Frame<'src, 'pcm>, InsufficientData> {
        self.peek_cache_len = None;
        unsafe {
            let pcm_ptr = (&mut self.decoder_buf) as *mut DecoderBuf as *mut Sample;
            let samples = self.dec(pcm_ptr);
            let frame_recv = &*self.frame_recv.as_ptr();
            let response = translate_response(frame_recv, samples, &self.source, |points| {
                (&*(pcm_ptr as *const [Sample; MAX_SAMPLES_PER_FRAME]))
                    .get_unchecked(..points)
            });

            if response.is_ok() {
                self.offset_trusted(frame_recv.frame_bytes as usize);
            }

            response
        }
    }

    /// Gets the current position in the input data.
    #[inline]
    pub fn position(&self) -> usize {
        unsafe { self.source.as_ptr().sub(self.source_copy.as_ptr() as usize) as usize }
    }

    /// Reads a frame without actually decoding it or advancing the iterator.
    /// This means that the samples will always be empty in a frame containing PCM data,
    /// therefore [`sample_count`](Samples::sample_count) should be used instead to inspect the length.
    ///
    /// Repeated calls to `peek` will return the same result.
    /// Use [`skip`](Self::skip) to advance without decoding.
    pub fn peek(&mut self) -> Result<Frame<'src, 'static>, InsufficientData> {
        self.peek_cache_len = None;
        unsafe {
            let samples = self.dec(ptr::null_mut());
            let frame_recv = &*self.frame_recv.as_ptr();
            let response = translate_response(frame_recv, samples, &self.source, |_| &[]);
            match &response {
                Ok(Frame::Audio(samples)) => self.peek_cache_len = Some(samples.bytes_consumed),
                Ok(Frame::Unknown { bytes_consumed, .. }) => {
                    self.peek_cache_len = Some(*bytes_consumed)
                }
                Err(_) => self.peek_cache_len = None,
            }
            response
        }
    }

    /// Sets the current position in the input data.
    ///
    /// If `position` is out of bounds, it's set to the end of the file instead.
    #[inline]
    pub fn set_position(&mut self, position: usize) {
        let position = self.source_copy.len().min(position);
        self.source = unsafe { self.source_copy.get_unchecked(position..) };
    }

    /// Skips the current frame the iterator is over, if any.
    pub fn skip(&mut self) -> Result<(), InsufficientData> {
        unsafe {
            let offset = match self.peek_cache_len.take() {
                Some(offset) => offset,
                None => match self.peek()? {
                    Frame::Audio(Samples { bytes_consumed, .. })
                    | Frame::Unknown { bytes_consumed, .. } => bytes_consumed,
                },
            };
            self.offset_trusted(offset);
        }
        Ok(())
    }

    #[inline(always)]
    unsafe fn dec(&mut self, pcm_out: *mut Sample) -> c_int {
        let data_len = data_len_safe(self.source.len());
        ffi::mp3dec_decode_frame(
            self.decoder.as_mut_ptr(),
            self.source.as_ptr(),
            data_len,
            pcm_out,
            self.frame_recv.as_mut_ptr(),
        )
    }

    #[inline(always)]
    unsafe fn offset_trusted(&mut self, offset: usize) {
        self.source = self.source.get_unchecked(offset..);
    }
}

#[cfg(feature = "std")]
impl DecoderStreamOwned {
    pub fn new(source: impl Into<Box<[u8]>>) -> Self {
        let data = source.into();
        let slice = unsafe { std::slice::from_raw_parts(data.as_ptr(), data.len()) };
        Self {
            _data: data,
            inner: DecoderStream::new(slice),
        }
    }

    pub fn peek<'src>(&'src mut self) -> Result<Frame<'src, 'static>, InsufficientData> {
        self.inner.peek()
    }

    pub fn next<'dec>(&'dec mut self) -> Result<Frame<'dec, 'dec>, InsufficientData> {
        self.inner.next()
    }

    pub fn skip(&mut self) -> Result<(), InsufficientData> {
        self.inner.skip()
    }
}

// The minimp3 API takes `int` for size, however that won't work if
// your file exceeds 2GB (usually 2^31-1 bytes) in size. Thankfully,
// under pretty much no circumstances will each frame be >2GB.
// Even if it would be, this makes it not UB and just return err/eof.
#[inline(always)]
unsafe fn data_len_safe(len: usize) -> c_int {
    len.min(c_int::max_value() as usize) as c_int
}

#[inline(always)]
unsafe fn translate_response<'src, 'pcm>(
    frame_recv: &ffi::mp3dec_frame_info_t,
    samples: c_int,
    source: &'src [u8],
    pcm_f: impl Fn(usize) -> &'pcm [Sample],
) -> Result<Frame<'src, 'pcm>, InsufficientData> {
    if samples != 0 {
        // we got samples!
        Ok(Frame::Audio(Samples {
            bitrate: frame_recv.bitrate_kbps as u32,
            channels: frame_recv.channels as u32,
            mpeg_layer: frame_recv.layer as u32,
            sample_rate: frame_recv.hz as u32,

            bytes_consumed: frame_recv.frame_bytes as usize,
            source: source_slice(source, frame_recv),
            samples: pcm_f(samples as usize * frame_recv.channels as usize),
            sample_count: samples as usize,
        }))
    } else if frame_recv.frame_bytes != 0 {
        // we got... something!
        Ok(Frame::Unknown {
            bytes_consumed: frame_recv.frame_bytes as usize,
            source: source_slice(source, frame_recv),
        })
    } else {
        // nope.
        Err(InsufficientData)
    }
}

/// Returns the source slice from a received mp3dec_frame_info_t.
#[inline(always)]
unsafe fn source_slice<'src, 'frame>(
    data: &'src [u8],
    frame_recv: &'frame ffi::mp3dec_frame_info_t,
) -> &'src [u8] {
    data.get_unchecked(frame_recv.frame_offset as usize..frame_recv.frame_bytes as usize)
}
