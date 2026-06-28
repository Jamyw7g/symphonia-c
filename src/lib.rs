use std::cell::RefCell;
use std::ffi::{CStr, CString};
use std::fs::File;
use std::os::raw::c_char;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::ptr;
use std::slice;

use symphonia::core::codecs::CodecParameters;
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::errors::Error;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

pub const SYMPHONIA_C_OK: i32 = 0;
pub const SYMPHONIA_C_END_OF_STREAM: i32 = 1;
pub const SYMPHONIA_C_INVALID_ARGUMENT: i32 = -1;
pub const SYMPHONIA_C_IO_ERROR: i32 = -2;
pub const SYMPHONIA_C_DECODE_ERROR: i32 = -3;
pub const SYMPHONIA_C_UNSUPPORTED: i32 = -4;
pub const SYMPHONIA_C_LIMIT_ERROR: i32 = -5;
pub const SYMPHONIA_C_INTERNAL_ERROR: i32 = -6;

static OK_MESSAGE: &[u8] = b"ok\0";
static END_OF_STREAM_MESSAGE: &[u8] = b"end of stream\0";
static INVALID_ARGUMENT_MESSAGE: &[u8] = b"invalid argument\0";
static IO_ERROR_MESSAGE: &[u8] = b"io error\0";
static DECODE_ERROR_MESSAGE: &[u8] = b"decode error\0";
static UNSUPPORTED_MESSAGE: &[u8] = b"unsupported format or codec\0";
static LIMIT_ERROR_MESSAGE: &[u8] = b"decoder limit reached\0";
static INTERNAL_ERROR_MESSAGE: &[u8] = b"internal error\0";
static UNKNOWN_STATUS_MESSAGE: &[u8] = b"unknown status\0";

thread_local! {
    static LAST_ERROR: RefCell<CString> = RefCell::new(c_string(""));
}

#[repr(C)]
pub struct SymphoniaCAudioBuffer {
    pub samples: *mut f32,
    pub sample_count: usize,
    pub frames: usize,
    pub channels: u32,
    pub sample_rate: u32,
}

pub struct SymphoniaCDecoder {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    sample_rate: u32,
    channels: u32,
    total_frames: u64,
    pending: Vec<f32>,
    pending_offset: usize,
    eof: bool,
    last_error: CString,
}

#[derive(Debug)]
struct ApiError {
    status: i32,
    message: String,
}

impl ApiError {
    fn new(status: i32, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn invalid_argument(message: impl Into<String>) -> Self {
        Self::new(SYMPHONIA_C_INVALID_ARGUMENT, message)
    }
}

impl From<std::io::Error> for ApiError {
    fn from(error: std::io::Error) -> Self {
        Self::new(SYMPHONIA_C_IO_ERROR, error.to_string())
    }
}

impl From<Error> for ApiError {
    fn from(error: Error) -> Self {
        let status = match &error {
            Error::IoError(_) => SYMPHONIA_C_IO_ERROR,
            Error::DecodeError(_) | Error::ResetRequired => SYMPHONIA_C_DECODE_ERROR,
            Error::SeekError(_) => SYMPHONIA_C_DECODE_ERROR,
            Error::Unsupported(_) => SYMPHONIA_C_UNSUPPORTED,
            Error::LimitError(_) => SYMPHONIA_C_LIMIT_ERROR,
            _ => SYMPHONIA_C_INTERNAL_ERROR,
        };

        Self::new(status, error.to_string())
    }
}

impl SymphoniaCDecoder {
    fn open_file(path: &Path) -> Result<Self, ApiError> {
        let file = Box::new(File::open(path)?);
        let mss = MediaSourceStream::new(file, Default::default());

        let mut hint = Hint::new();
        if let Some(extension) = path.extension().and_then(|value| value.to_str()) {
            hint.with_extension(extension);
        }

        let fmt_opts = FormatOptions::default();
        let meta_opts = MetadataOptions::default();
        let format = symphonia::default::get_probe().probe(&hint, mss, fmt_opts, meta_opts)?;

        let (track_id, sample_rate, channels, total_frames, decoder) = {
            let track = format
                .default_track(TrackType::Audio)
                .ok_or_else(|| ApiError::new(SYMPHONIA_C_UNSUPPORTED, "no audio track found"))?;

            let params = match track.codec_params.as_ref() {
                Some(CodecParameters::Audio(params)) => params,
                _ => {
                    return Err(ApiError::new(
                        SYMPHONIA_C_UNSUPPORTED,
                        "audio track is missing codec parameters",
                    ));
                }
            };

            let decoder = symphonia::default::get_codecs()
                .make_audio_decoder(params, &AudioDecoderOptions::default())?;

            (
                track.id,
                params.sample_rate.unwrap_or(0),
                params
                    .channels
                    .as_ref()
                    .map(|channels| channels.count() as u32)
                    .unwrap_or(0),
                track.num_frames.unwrap_or(0),
                decoder,
            )
        };

        Ok(Self {
            format,
            decoder,
            track_id,
            sample_rate,
            channels,
            total_frames,
            pending: Vec::new(),
            pending_offset: 0,
            eof: false,
            last_error: c_string(""),
        })
    }

    fn read_f32(&mut self, out: &mut [f32], frame_capacity: usize) -> Result<usize, ApiError> {
        if frame_capacity == 0 {
            return Ok(0);
        }

        let channels = self.channel_count()?;
        let sample_capacity = frame_capacity
            .checked_mul(channels)
            .ok_or_else(|| ApiError::invalid_argument("frame capacity overflows sample count"))?;

        if out.len() < sample_capacity {
            return Err(ApiError::invalid_argument("output buffer is too small"));
        }

        let mut written = 0;

        while written < sample_capacity {
            written += self.drain_pending(&mut out[written..sample_capacity]);

            if written >= sample_capacity || self.eof {
                break;
            }

            match self.decode_next_samples()? {
                Some(samples) if !samples.is_empty() => {
                    self.pending = samples;
                    self.pending_offset = 0;
                }
                Some(_) => {}
                None => {
                    self.eof = true;
                    break;
                }
            }
        }

        Ok(written / channels)
    }

    fn channel_count(&self) -> Result<usize, ApiError> {
        if self.channels == 0 {
            return Err(ApiError::new(
                SYMPHONIA_C_UNSUPPORTED,
                "audio channel count is unknown",
            ));
        }

        Ok(self.channels as usize)
    }

    fn drain_pending(&mut self, out: &mut [f32]) -> usize {
        let remaining = self.pending.len().saturating_sub(self.pending_offset);
        let count = remaining.min(out.len());

        if count > 0 {
            let start = self.pending_offset;
            let end = start + count;
            out[..count].copy_from_slice(&self.pending[start..end]);
            self.pending_offset = end;

            if self.pending_offset == self.pending.len() {
                self.pending.clear();
                self.pending_offset = 0;
            }
        }

        count
    }

    fn decode_next_samples(&mut self) -> Result<Option<Vec<f32>>, ApiError> {
        loop {
            let packet = match self.format.next_packet()? {
                Some(packet) => packet,
                None => return Ok(None),
            };

            if packet.track_id != self.track_id {
                continue;
            }

            match self.decoder.decode(&packet) {
                Ok(audio_buf) => {
                    let spec = audio_buf.spec();
                    self.sample_rate = spec.rate();
                    self.channels = spec.channels().count() as u32;

                    let mut samples = vec![0.0; audio_buf.samples_interleaved()];
                    audio_buf.copy_to_slice_interleaved(&mut samples);
                    return Ok(Some(samples));
                }
                Err(Error::DecodeError(_)) => continue,
                Err(Error::ResetRequired) => {
                    self.decoder.reset();
                    continue;
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    fn set_error(&mut self, message: impl AsRef<str>) {
        self.last_error = c_string(message.as_ref());
    }

    fn clear_error(&mut self) {
        self.set_error("");
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn symphonia_c_open_file(
    path: *const c_char,
    out_decoder: *mut *mut SymphoniaCDecoder,
) -> i32 {
    match catch_unwind(AssertUnwindSafe(|| open_file_impl(path, out_decoder))) {
        Ok(Ok(())) => SYMPHONIA_C_OK,
        Ok(Err(error)) => {
            set_global_error(&error.message);
            error.status
        }
        Err(_) => {
            set_global_error("panic while opening decoder");
            SYMPHONIA_C_INTERNAL_ERROR
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn symphonia_c_free(decoder: *mut SymphoniaCDecoder) {
    if !decoder.is_null() {
        unsafe {
            drop(Box::from_raw(decoder));
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn symphonia_c_sample_rate(decoder: *const SymphoniaCDecoder) -> u32 {
    let Some(decoder) = (unsafe { decoder.as_ref() }) else {
        return 0;
    };

    decoder.sample_rate
}

#[unsafe(no_mangle)]
pub extern "C" fn symphonia_c_channels(decoder: *const SymphoniaCDecoder) -> u32 {
    let Some(decoder) = (unsafe { decoder.as_ref() }) else {
        return 0;
    };

    decoder.channels
}

#[unsafe(no_mangle)]
pub extern "C" fn symphonia_c_total_frames(decoder: *const SymphoniaCDecoder) -> u64 {
    let Some(decoder) = (unsafe { decoder.as_ref() }) else {
        return 0;
    };

    decoder.total_frames
}

#[unsafe(no_mangle)]
pub extern "C" fn symphonia_c_read_f32(
    decoder: *mut SymphoniaCDecoder,
    out_samples: *mut f32,
    frame_capacity: usize,
    out_frames: *mut usize,
) -> i32 {
    match catch_unwind(AssertUnwindSafe(|| {
        read_f32_impl(decoder, out_samples, frame_capacity, out_frames)
    })) {
        Ok(Ok(status)) => status,
        Ok(Err(error)) => {
            set_decoder_or_global_error(decoder, &error.message);
            error.status
        }
        Err(_) => {
            set_decoder_or_global_error(decoder, "panic while reading decoder");
            SYMPHONIA_C_INTERNAL_ERROR
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn symphonia_c_decode_file_f32(
    path: *const c_char,
    out_buffer: *mut SymphoniaCAudioBuffer,
) -> i32 {
    match catch_unwind(AssertUnwindSafe(|| decode_file_f32_impl(path, out_buffer))) {
        Ok(Ok(())) => SYMPHONIA_C_OK,
        Ok(Err(error)) => {
            set_global_error(&error.message);
            error.status
        }
        Err(_) => {
            set_global_error("panic while decoding file");
            SYMPHONIA_C_INTERNAL_ERROR
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn symphonia_c_audio_buffer_free(buffer: *mut SymphoniaCAudioBuffer) {
    let Some(buffer) = (unsafe { buffer.as_mut() }) else {
        return;
    };

    if !buffer.samples.is_null() && buffer.sample_count > 0 {
        unsafe {
            drop(Box::from_raw(ptr::slice_from_raw_parts_mut(
                buffer.samples,
                buffer.sample_count,
            )));
        }
    }

    *buffer = empty_audio_buffer();
}

#[unsafe(no_mangle)]
pub extern "C" fn symphonia_c_last_error(decoder: *const SymphoniaCDecoder) -> *const c_char {
    if let Some(decoder) = unsafe { decoder.as_ref() } {
        return decoder.last_error.as_ptr();
    }

    LAST_ERROR.with(|last_error| last_error.borrow().as_ptr())
}

#[unsafe(no_mangle)]
pub extern "C" fn symphonia_c_status_message(status: i32) -> *const c_char {
    match status {
        SYMPHONIA_C_OK => OK_MESSAGE,
        SYMPHONIA_C_END_OF_STREAM => END_OF_STREAM_MESSAGE,
        SYMPHONIA_C_INVALID_ARGUMENT => INVALID_ARGUMENT_MESSAGE,
        SYMPHONIA_C_IO_ERROR => IO_ERROR_MESSAGE,
        SYMPHONIA_C_DECODE_ERROR => DECODE_ERROR_MESSAGE,
        SYMPHONIA_C_UNSUPPORTED => UNSUPPORTED_MESSAGE,
        SYMPHONIA_C_LIMIT_ERROR => LIMIT_ERROR_MESSAGE,
        SYMPHONIA_C_INTERNAL_ERROR => INTERNAL_ERROR_MESSAGE,
        _ => UNKNOWN_STATUS_MESSAGE,
    }
    .as_ptr()
    .cast()
}

fn open_file_impl(
    path: *const c_char,
    out_decoder: *mut *mut SymphoniaCDecoder,
) -> Result<(), ApiError> {
    if out_decoder.is_null() {
        return Err(ApiError::invalid_argument("out_decoder must not be null"));
    }

    unsafe {
        *out_decoder = ptr::null_mut();
    }

    let path = c_path(path)?;
    let decoder = SymphoniaCDecoder::open_file(Path::new(&path))?;

    unsafe {
        *out_decoder = Box::into_raw(Box::new(decoder));
    }

    set_global_error("");
    Ok(())
}

fn read_f32_impl(
    decoder: *mut SymphoniaCDecoder,
    out_samples: *mut f32,
    frame_capacity: usize,
    out_frames: *mut usize,
) -> Result<i32, ApiError> {
    if out_frames.is_null() {
        return Err(ApiError::invalid_argument("out_frames must not be null"));
    }

    unsafe {
        *out_frames = 0;
    }

    let decoder = unsafe { decoder.as_mut() }
        .ok_or_else(|| ApiError::invalid_argument("decoder must not be null"))?;
    decoder.clear_error();

    if frame_capacity == 0 {
        return Ok(SYMPHONIA_C_OK);
    }

    let channels = decoder.channel_count()?;
    let sample_capacity = frame_capacity
        .checked_mul(channels)
        .ok_or_else(|| ApiError::invalid_argument("frame capacity overflows sample count"))?;

    if out_samples.is_null() {
        return Err(ApiError::invalid_argument("out_samples must not be null"));
    }

    let out = unsafe { slice::from_raw_parts_mut(out_samples, sample_capacity) };
    let frames_read = decoder.read_f32(out, frame_capacity)?;

    unsafe {
        *out_frames = frames_read;
    }

    if frames_read == 0 && decoder.eof {
        Ok(SYMPHONIA_C_END_OF_STREAM)
    } else {
        Ok(SYMPHONIA_C_OK)
    }
}

fn decode_file_f32_impl(
    path: *const c_char,
    out_buffer: *mut SymphoniaCAudioBuffer,
) -> Result<(), ApiError> {
    let out_buffer = unsafe { out_buffer.as_mut() }
        .ok_or_else(|| ApiError::invalid_argument("out_buffer must not be null"))?;
    *out_buffer = empty_audio_buffer();

    let path = c_path(path)?;
    let mut decoder = SymphoniaCDecoder::open_file(Path::new(&path))?;
    let channels = decoder.channel_count()?;
    let mut samples = Vec::new();
    let chunk_frames = 4096;

    loop {
        let start = samples.len();
        samples.resize(start + chunk_frames * channels, 0.0);
        let frames_read = decoder.read_f32(&mut samples[start..], chunk_frames)?;
        samples.truncate(start + frames_read * channels);

        if frames_read == 0 && decoder.eof {
            break;
        }
    }

    out_buffer.frames = samples.len() / channels;
    out_buffer.channels = decoder.channels;
    out_buffer.sample_rate = decoder.sample_rate;
    out_buffer.sample_count = samples.len();

    if !samples.is_empty() {
        let boxed = samples.into_boxed_slice();
        out_buffer.samples = Box::into_raw(boxed) as *mut f32;
    }

    set_global_error("");
    Ok(())
}

fn c_path(path: *const c_char) -> Result<String, ApiError> {
    if path.is_null() {
        return Err(ApiError::invalid_argument("path must not be null"));
    }

    let path = unsafe { CStr::from_ptr(path) };
    path.to_str()
        .map(str::to_owned)
        .map_err(|_| ApiError::invalid_argument("path must be valid UTF-8"))
}

fn empty_audio_buffer() -> SymphoniaCAudioBuffer {
    SymphoniaCAudioBuffer {
        samples: ptr::null_mut(),
        sample_count: 0,
        frames: 0,
        channels: 0,
        sample_rate: 0,
    }
}

fn set_decoder_or_global_error(decoder: *mut SymphoniaCDecoder, message: impl AsRef<str>) {
    if let Some(decoder) = unsafe { decoder.as_mut() } {
        decoder.set_error(message);
    } else {
        set_global_error(message);
    }
}

fn set_global_error(message: impl AsRef<str>) {
    LAST_ERROR.with(|last_error| {
        *last_error.borrow_mut() = c_string(message.as_ref());
    });
}

fn c_string(message: impl AsRef<str>) -> CString {
    let bytes = message
        .as_ref()
        .as_bytes()
        .iter()
        .map(|byte| if *byte == 0 { b' ' } else { *byte })
        .collect::<Vec<_>>();

    CString::new(bytes).unwrap_or_else(|_| CString::new("invalid string").expect("static string"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn decodes_wav_file_to_interleaved_f32() {
        let path = write_test_wav("decode_file");
        let c_path = CString::new(path.to_string_lossy().as_bytes()).unwrap();
        let mut buffer = empty_audio_buffer();

        let status = symphonia_c_decode_file_f32(c_path.as_ptr(), &mut buffer);
        assert_eq!(status, SYMPHONIA_C_OK, "{}", last_error(ptr::null()));
        assert_eq!(buffer.sample_rate, 44_100);
        assert_eq!(buffer.channels, 2);
        assert_eq!(buffer.frames, 4);
        assert_eq!(buffer.sample_count, 8);

        let samples = unsafe { slice::from_raw_parts(buffer.samples, buffer.sample_count) };
        assert!(samples[2] > 0.9);
        assert!(samples[3] < -0.9);

        symphonia_c_audio_buffer_free(&mut buffer);
        assert!(buffer.samples.is_null());

        let _ = fs::remove_file(path);
    }

    #[test]
    fn streams_wav_file_in_frame_chunks() {
        let path = write_test_wav("stream");
        let c_path = CString::new(path.to_string_lossy().as_bytes()).unwrap();
        let mut decoder = ptr::null_mut();

        let status = symphonia_c_open_file(c_path.as_ptr(), &mut decoder);
        assert_eq!(status, SYMPHONIA_C_OK, "{}", last_error(ptr::null()));
        assert_eq!(symphonia_c_sample_rate(decoder), 44_100);
        assert_eq!(symphonia_c_channels(decoder), 2);

        let mut samples = [0.0; 4];
        let mut frames = 0;
        let status = symphonia_c_read_f32(decoder, samples.as_mut_ptr(), 2, &mut frames);
        assert_eq!(status, SYMPHONIA_C_OK, "{}", last_error(decoder));
        assert_eq!(frames, 2);
        assert!(samples[2] > 0.9);
        assert!(samples[3] < -0.9);

        let status = symphonia_c_read_f32(decoder, samples.as_mut_ptr(), 2, &mut frames);
        assert_eq!(status, SYMPHONIA_C_OK, "{}", last_error(decoder));
        assert_eq!(frames, 2);

        let status = symphonia_c_read_f32(decoder, samples.as_mut_ptr(), 2, &mut frames);
        assert_eq!(status, SYMPHONIA_C_END_OF_STREAM);
        assert_eq!(frames, 0);

        symphonia_c_free(decoder);
        let _ = fs::remove_file(path);
    }

    fn write_test_wav(name: &str) -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("symphonia-c-{name}-{}.wav", std::process::id()));
        fs::write(&path, make_test_wav()).unwrap();
        path
    }

    fn make_test_wav() -> Vec<u8> {
        let channels = 2u16;
        let sample_rate = 44_100u32;
        let bits_per_sample = 16u16;
        let block_align = channels * bits_per_sample / 8;
        let byte_rate = sample_rate * u32::from(block_align);
        let pcm_samples = [0i16, 0, i16::MAX, i16::MIN, 16_384, -16_384, 0, 0];
        let data_size = (pcm_samples.len() * std::mem::size_of::<i16>()) as u32;

        let mut wav = Vec::with_capacity(44 + data_size as usize);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_size).to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&channels.to_le_bytes());
        wav.extend_from_slice(&sample_rate.to_le_bytes());
        wav.extend_from_slice(&byte_rate.to_le_bytes());
        wav.extend_from_slice(&block_align.to_le_bytes());
        wav.extend_from_slice(&bits_per_sample.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_size.to_le_bytes());

        for sample in pcm_samples {
            wav.extend_from_slice(&sample.to_le_bytes());
        }

        wav
    }

    fn last_error(decoder: *const SymphoniaCDecoder) -> String {
        unsafe {
            CStr::from_ptr(symphonia_c_last_error(decoder))
                .to_string_lossy()
                .into_owned()
        }
    }
}
