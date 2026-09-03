#![warn(rust_2018_idioms)]
#![warn(rust_2021_compatibility)]
#![warn(clippy::missing_panics_doc)]
#![warn(clippy::clone_on_ref_ptr)]
#![deny(trivial_numeric_casts)]
#![forbid(unsafe_code)]

use std::fs::File;
use std::path::PathBuf;

use symphonia::core::audio::{Audio, AudioBuffer};
use symphonia::core::codecs::audio::{
    AudioCodecParameters, AudioDecoder as SymphDecoder, AudioDecoderOptions,
};
use symphonia::core::codecs::CodecParameters;
use symphonia::core::errors::Error;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{Metadata, MetadataOptions, MetadataRevision};
use symphonia::core::units::{TimeBase, Timestamp};

use creek_core::{DataBlock, Decoder, FileInfo};

mod error;
pub use error::OpenError;

pub struct SymphoniaDecoder {
    reader: Box<dyn FormatReader>,
    decoder: Box<dyn SymphDecoder>,

    decode_buffer: AudioBuffer<f32>,
    decode_buffer_len: usize,
    curr_decode_buffer_frame: usize,

    num_frames: usize,
    sample_rate: Option<u32>,
    /// The audio track's timebase and id, used to turn a frame index into a seek timestamp.
    time_base: Option<TimeBase>,
    track_id: u32,
    block_size: usize,

    playhead_frame: usize,
    reset_decode_buffer: bool,
    seek_diff: usize,
}

impl Decoder for SymphoniaDecoder {
    type T = f32;
    type FileParams = SymphoniaDecoderInfo;
    type OpenError = OpenError;
    type FatalError = Error;
    type AdditionalOpts = ();

    const DEFAULT_BLOCK_SIZE: usize = 16384;
    const DEFAULT_NUM_CACHE_BLOCKS: usize = 0;
    const DEFAULT_NUM_LOOK_AHEAD_BLOCKS: usize = 8;

    fn new(
        file: PathBuf,
        start_frame: usize,
        block_size: usize,
        _additional_opts: Self::AdditionalOpts,
    ) -> Result<(Self, FileInfo<Self::FileParams>), Self::OpenError> {
        // Create a hint to help the format registry guess what format reader is appropriate.
        let mut hint = Hint::new();

        // Provide the file extension as a hint.
        if let Some(extension) = file.extension() {
            if let Some(extension_str) = extension.to_str() {
                hint.with_extension(extension_str);
            }
        }

        let source = Box::new(File::open(file)?);

        // Create the media source stream using the boxed media source from above.
        let mss = MediaSourceStream::new(source, Default::default());

        // Use the default options for metadata and format readers.
        let format_opts: FormatOptions = Default::default();
        let metadata_opts: MetadataOptions = Default::default();

        let mut reader =
            symphonia::default::get_probe().probe(&hint, mss, format_opts, metadata_opts)?;

        let decoder_opts = AudioDecoderOptions::default();

        let (params, num_frames, time_base, track_id) = {
            // Get the default audio track.
            let track = reader
                .default_track(TrackType::Audio)
                .ok_or(OpenError::NoDefaultTrack)?;

            let params = match &track.codec_params {
                Some(CodecParameters::Audio(params)) => params.clone(),
                _ => return Err(OpenError::NoDefaultTrack),
            };
            let num_frames = track.num_frames.ok_or(OpenError::NoNumFrames)? as usize;

            (params, num_frames, track.time_base, track.id)
        };
        let sample_rate = params.sample_rate;

        // Seek the reader to the requested position.
        if start_frame != 0 {
            reader.seek(
                SeekMode::Accurate,
                SeekTo::Timestamp {
                    ts: frame_to_ts(start_frame as u64, sample_rate.unwrap_or(44100), time_base),
                    track_id,
                },
            )?;
        }

        // Create a decoder for the stream.
        let mut decoder =
            symphonia::default::get_codecs().make_audio_decoder(&params, &decoder_opts)?;
        debug_assert_eq!(params.sample_rate, decoder.codec_params().sample_rate);
        debug_assert_eq!(params.channels, decoder.codec_params().channels);

        // The stream/decoder might not always provide the actual numbers
        // of channels (MP4/AAC/ALAC). In this case the number of channels
        // will be obtained from the audio spec of the first decoded packet.
        let mut channels = params.channels.clone();

        // Decode the first packet to get the audio specification.
        let (decode_buffer, decode_buffer_len) = loop {
            let Some(packet) = reader.next_packet()? else {
                return Err(OpenError::NoAudioPackets);
            };

            match decoder.decode(&packet) {
                Ok(decoded) => {
                    // Get the buffer spec.
                    let spec = decoded.spec().clone();
                    if let Some(channels) = &channels {
                        assert_eq!(channels, spec.channels());
                    } else {
                        log::debug!(
                            "Assuming {num_channels} channel(s) according to the first decoded packet",
                            num_channels = spec.channels().count()
                        );
                        channels = Some(spec.channels().clone());
                    }

                    let len = decoded.frames();
                    let capacity = decoded.capacity();

                    let mut decode_buffer: AudioBuffer<f32> = AudioBuffer::new(spec, capacity);

                    decode_buffer.render_uninit(Some(len));
                    decoded.copy_to(&mut decode_buffer);

                    break (decode_buffer, len);
                }
                Err(Error::DecodeError(err)) => {
                    // Decode errors are not fatal.
                    log::warn!("{err}");
                    // Continue by decoding the next packet.
                    continue;
                }
                Err(e) => {
                    // Errors other than decode errors are fatal.
                    return Err(e.into());
                }
            }
        };

        let metadata = reader.metadata().skip_to_latest().cloned();
        let info = SymphoniaDecoderInfo {
            codec_params: params,
            metadata,
        };
        let num_channels = (channels.ok_or(OpenError::NoNumChannels)?).count();

        let file_info = FileInfo {
            params: info,
            num_frames,
            num_channels: num_channels as u16,
            sample_rate,
        };
        Ok((
            Self {
                reader,
                decoder,

                decode_buffer,
                decode_buffer_len,
                curr_decode_buffer_frame: 0,

                num_frames,
                sample_rate,
                time_base,
                track_id,
                block_size,

                playhead_frame: start_frame,
                reset_decode_buffer: false,
                seek_diff: 0,
            },
            file_info,
        ))
    }

    fn seek(&mut self, frame: usize) -> Result<(), Self::FatalError> {
        if frame >= self.num_frames {
            // Do nothing if out of range.
            self.playhead_frame = self.num_frames;

            return Ok(());
        }

        self.playhead_frame = frame;

        match self.reader.seek(
            SeekMode::Accurate,
            SeekTo::Timestamp {
                ts: frame_to_ts(
                    self.playhead_frame as u64,
                    self.sample_rate.unwrap_or(44100),
                    self.time_base,
                ),
                track_id: self.track_id,
            },
        ) {
            Ok(res) => {
                // this is always correct for `SeekMode::Accurate`, it may not be for `SeekMode::Coarse`
                debug_assert!(res.required_ts >= res.actual_ts);

                self.seek_diff = (res.required_ts.get() - res.actual_ts.get()) as usize;
            }
            Err(e) => {
                return Err(e);
            }
        }

        self.decoder.reset();

        self.reset_decode_buffer = true;
        self.curr_decode_buffer_frame = 0;

        Ok(())
    }

    fn decode(&mut self, data_block: &mut DataBlock<Self::T>) -> Result<(), Self::FatalError> {
        if self.playhead_frame >= self.num_frames {
            // Do nothing if reached the end of the file.
            return Ok(());
        }

        let mut reached_end_of_file = false;

        let mut block_start_frame = 0;
        while block_start_frame < self.block_size {
            let num_frames_to_cpy = if self.reset_decode_buffer {
                // Get new data first.
                self.reset_decode_buffer = false;
                0
            } else {
                // Find the maximum amount of frames that can be copied.
                (self.block_size - block_start_frame)
                    .min(self.decode_buffer_len - self.curr_decode_buffer_frame)
            };

            if num_frames_to_cpy != 0 {
                for (dst_ch, src_ch) in data_block
                    .block
                    .iter_mut()
                    .zip(self.decode_buffer.iter_planes())
                {
                    let src_ch_part = &src_ch[self.curr_decode_buffer_frame
                        ..self.curr_decode_buffer_frame + num_frames_to_cpy];
                    dst_ch.extend_from_slice(src_ch_part);
                }

                block_start_frame += num_frames_to_cpy;

                self.curr_decode_buffer_frame += num_frames_to_cpy;
                if self.curr_decode_buffer_frame >= self.decode_buffer_len {
                    self.reset_decode_buffer = true;
                }
            } else {
                // Decode the next packet.

                loop {
                    match self.reader.next_packet() {
                        Ok(Some(packet)) => {
                            match self.decoder.decode(&packet) {
                                Ok(decoded) => {
                                    self.decode_buffer_len = decoded.frames();
                                    if self.seek_diff < self.decode_buffer_len {
                                        let capacity = decoded.capacity();
                                        if self.decode_buffer.capacity() < capacity {
                                            self.decode_buffer =
                                                AudioBuffer::new(decoded.spec().clone(), capacity);
                                        }
                                        self.decode_buffer.clear();
                                        self.decode_buffer
                                            .render_uninit(Some(self.decode_buffer_len));
                                        decoded.copy_to(&mut self.decode_buffer);
                                        self.curr_decode_buffer_frame = self.seek_diff;
                                        self.seek_diff = 0;
                                        break;
                                    } else {
                                        self.seek_diff -= self.decode_buffer_len;
                                    }
                                }
                                Err(Error::DecodeError(err)) => {
                                    // Decode errors are not fatal.
                                    log::warn!("{err}");
                                    // Continue by decoding the next packet.
                                    continue;
                                }
                                Err(e) => {
                                    // Errors other than decode errors are fatal.
                                    return Err(e);
                                }
                            }
                        }
                        Ok(None) => {
                            reached_end_of_file = true;
                            block_start_frame = self.block_size;
                            break;
                        }
                        Err(e) => {
                            return Err(e);
                        }
                    }
                }
            }
        }

        if reached_end_of_file {
            self.playhead_frame = self.num_frames;
        } else {
            self.playhead_frame += self.block_size;
        }

        Ok(())
    }

    fn current_frame(&self) -> usize {
        self.playhead_frame
    }
}

impl Drop for SymphoniaDecoder {
    fn drop(&mut self) {
        let _ = self.decoder.finalize();
    }
}

impl SymphoniaDecoder {
    /// Symphonia does metadata oddly. This is more for raw access.
    ///
    /// See [`Metadata`](https://docs.rs/symphonia-core/latest/symphonia_core/meta/struct.Metadata.html).
    pub fn get_metadata_raw(&mut self) -> Metadata<'_> {
        self.reader.metadata()
    }

    /// Get the latest entry in the metadata.
    pub fn get_metadata(&mut self) -> Option<MetadataRevision> {
        let mut md = self.reader.metadata();
        md.skip_to_latest().cloned()
    }
}

#[derive(Debug, Clone)]
pub struct SymphoniaDecoderInfo {
    pub codec_params: AudioCodecParameters,
    pub metadata: Option<MetadataRevision>,
}

/// Converts an audio-frame index into a timestamp in the track's timebase units, for use with
/// [`SeekTo::Timestamp`].
///
/// One audio frame lasts `1 / sample_rate` seconds and one timebase tick lasts `numer / denom`
/// seconds, so one frame is `denom / (numer * sample_rate)` ticks. For the usual audio timebase of
/// `1 / sample_rate` this is exactly `frame`. The division is rounded to the nearest tick.
fn frame_to_ts(frame: u64, sample_rate: u32, time_base: Option<TimeBase>) -> Timestamp {
    let ticks = match time_base {
        Some(tb) => {
            let numer = u128::from(tb.numer.get()) * u128::from(sample_rate.max(1));
            let denom = u128::from(tb.denom.get());
            (u128::from(frame) * denom + numer / 2) / numer
        }
        // Without a timebase, assume one tick per audio frame.
        None => u128::from(frame),
    };

    Timestamp::new(i64::try_from(ticks).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;
    use float_cmp::*;

    #[test]
    fn decoder_new() {
        let files = vec![
            //  file | num_channels | num_frames | sample_rate
            ("../test_files/wav_u8_mono.wav", 1, 1323000, Some(44100)),
            ("../test_files/wav_i16_mono.wav", 1, 1323000, Some(44100)),
            ("../test_files/wav_i24_mono.wav", 1, 1323000, Some(44100)),
            ("../test_files/wav_i32_mono.wav", 1, 1323000, Some(44100)),
            ("../test_files/wav_f32_mono.wav", 1, 1323000, Some(44100)),
            ("../test_files/wav_i24_stereo.wav", 2, 1323000, Some(44100)),
            //"../test_files/ogg_mono.ogg",
            //"../test_files/ogg_stereo.ogg",
            //"../test_files/mp3_constant_mono.mp3",
            //"../test_files/mp3_constant_stereo.mp3",
            //"../test_files/mp3_variable_mono.mp3",
            //"../test_files/mp3_variable_stereo.mp3",
        ];

        for file in files {
            dbg!(file.0);
            let decoder =
                SymphoniaDecoder::new(file.0.into(), 0, SymphoniaDecoder::DEFAULT_BLOCK_SIZE, ());
            match decoder {
                Ok((_, file_info)) => {
                    assert_eq!(file_info.num_channels, file.1);
                    assert_eq!(file_info.num_frames, file.2);
                    //assert_eq!(file_info.sample_rate, file.3);
                }
                Err(e) => {
                    panic!("{}", e);
                }
            }
        }
    }

    #[test]
    fn decode_first_frame() {
        let block_size = 10;

        let decoder =
            SymphoniaDecoder::new("../test_files/wav_u8_mono.wav".into(), 0, block_size, ());

        let (mut decoder, file_info) = decoder.unwrap();

        let mut data_block = DataBlock::new(1, block_size);
        data_block.clear();
        decoder.decode(&mut data_block).unwrap();

        let samples = &mut data_block.block[0];
        assert_eq!(samples.len(), block_size);

        let first_frame = [
            0.0, 0.046875, 0.09375, 0.1484375, 0.1953125, 0.2421875, 0.2890625, 0.3359375,
            0.3828125, 0.421875,
        ];

        for i in 0..samples.len() {
            assert!(approx_eq!(f32, first_frame[i], samples[i], ulps = 2));
        }

        let second_frame = [
            0.46875, 0.5078125, 0.5390625, 0.578125, 0.609375, 0.640625, 0.671875, 0.6953125,
            0.71875, 0.7421875,
        ];

        data_block.clear();
        decoder.decode(&mut data_block).unwrap();

        let samples = &mut data_block.block[0];
        for i in 0..samples.len() {
            assert_approx_eq!(f32, second_frame[i], samples[i], ulps = 2);
        }

        let last_frame = [
            -0.0859375, -0.09375, -0.1015625, -0.1015625, -0.1015625, -0.09375, -0.0859375,
            -0.078125, -0.0625, -0.046875,
        ];

        // Seek to last frame
        decoder.seek(file_info.num_frames - 1 - block_size).unwrap();

        data_block.clear();
        decoder.decode(&mut data_block).unwrap();

        let samples = &mut data_block.block[0];
        for i in 0..samples.len() {
            assert_approx_eq!(f32, last_frame[i], samples[i], ulps = 2);
        }

        assert_eq!(decoder.playhead_frame, file_info.num_frames - 1);
    }

    #[test]
    fn seek_is_frame_accurate() {
        let block_size = 16;
        let path = "../test_files/wav_i16_mono.wav";

        let (mut decoder, file_info) =
            SymphoniaDecoder::new(path.into(), 0, block_size, ()).unwrap();

        // Decode the whole file sequentially to use as the reference.
        let mut reference: Vec<f32> = Vec::with_capacity(file_info.num_frames);
        let mut data_block = DataBlock::new(1, block_size);
        while reference.len() < file_info.num_frames {
            data_block.clear();
            decoder.decode(&mut data_block).unwrap();
            reference.extend_from_slice(&data_block.block[0]);
        }
        reference.truncate(file_info.num_frames);

        // A spread of target frames, including ones that landed one frame early with the old
        // `SeekTo::Time` conversion (2, 3, 7, ...) at 44.1 kHz.
        let targets = [
            1,
            2,
            3,
            7,
            100,
            4095,
            4096,
            4097,
            44_099,
            44_100,
            44_101,
            file_info.num_frames / 2,
            file_info.num_frames / 2 + 1,
            file_info.num_frames - block_size - 1,
        ];

        for frame in targets {
            decoder.seek(frame).unwrap();
            assert_eq!(decoder.current_frame(), frame);

            data_block.clear();
            decoder.decode(&mut data_block).unwrap();

            for (i, &sample) in data_block.block[0].iter().enumerate() {
                assert!(
                    approx_eq!(f32, reference[frame + i], sample, ulps = 2),
                    "mismatch after seek to frame {frame}, offset {i}: expected {}, got {sample}",
                    reference[frame + i],
                );
            }
        }
    }
}
