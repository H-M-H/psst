use std::sync::{Arc, Mutex};

use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    Device, SampleFormat, SampleRate, Stream, StreamConfig, SupportedStreamConfig,
};
use crossbeam_channel::{unbounded, Receiver, Sender};
use dasp::{interpolate::linear::Linear, sample::FromSample, signal, Frame, Sample, Signal};

use crate::error::Error;

pub type AudioSample = f32;

pub trait AudioSource: Iterator<Item = AudioSample> {
    fn channels(&self) -> u8;
    fn sample_rate(&self) -> u32;
    fn normalization_factor(&self) -> Option<f32>;
}

pub struct AudioOutputRemote {
    event_sender: Sender<InternalEvent>,
}

impl AudioOutputRemote {
    pub fn close(&self) {
        self.send(InternalEvent::Close);
    }

    pub fn pause(&self) {
        self.send(InternalEvent::Pause);
    }

    pub fn resume(&self) {
        self.send(InternalEvent::Resume);
    }

    pub fn set_volume(&self, volume: f64) {
        self.send(InternalEvent::SetVolume(volume));
    }

    fn send(&self, event: InternalEvent) {
        self.event_sender.send(event).expect("Audio output died");
    }
}

pub struct AudioOutput {
    device: Device,
    event_sender: Sender<InternalEvent>,
    event_receiver: Receiver<InternalEvent>,
}

impl AudioOutput {
    pub fn open() -> Result<Self, Error> {
        let device = cpal::default_host()
            .default_output_device()
            .ok_or_else(|| {
                Error::AudioOutputError(Box::new(CPalError(
                    "Failed to obtain default audio output device".into(),
                )))
            })?;

        // Channel used for controlling the audio output.
        let (event_sender, event_receiver) = unbounded();

        Ok(Self {
            device,
            event_sender,
            event_receiver,
        })
    }

    pub fn remote(&self) -> AudioOutputRemote {
        AudioOutputRemote {
            event_sender: self.event_sender.clone(),
        }
    }

    pub fn start_playback<T>(&self, source: Arc<Mutex<T>>) -> Result<(), Error>
    where
        T: AudioSource + Send + 'static,
    {
        // Create a device config that describes the kind of device we want to use.
        let config;

        {
            // Setup the device config for playback with the channel count and sample rate
            // from the audio source.
            let source = source.lock().expect("Failed to acquire audio source lock");
            config = StreamConfig {
                channels: source.channels().into(),
                sample_rate: SampleRate(source.sample_rate()),
                buffer_size: cpal::BufferSize::Default,
            };
        }

        let volume = Arc::new(Mutex::new(1.0f32));
        // Move the source and a clone of the volume into the data callback.
        let stream = {
            let volume = volume.clone();
            let source = source.clone();
            // try to build a stream with the config values from the audio source
            // this may fail, especially on windows
            self.device.build_output_stream(
                &config,
                move |output: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    let mut source = source.lock().expect("Failed to acquire audio source lock");
                    let norm_factor = source.normalization_factor().unwrap_or(1.0);
                    let volume = *volume.lock().unwrap();
                    // Fill the buffer with audio samples from the source.
                    for sample in output.iter_mut() {
                        let s = source.next().unwrap_or(0.0); // Use silence in case the
                                                              // source has finished.
                        *sample = s * norm_factor * volume;
                    }
                },
                |err| log::error!("an error occurred on stream: {}", err),
            )
        };

        let stream = match stream {
            Ok(stream) => stream,
            // If building the stream failed using the config values from the audio source,
            // try to build a new stream using the default config, which hopefully works,
            // and convert the audio source to match said config.
            Err(err) => {
                log::info!(
                    "Audio device can't play requested format ({}), performing conversion.",
                    err,
                );
                let config = self.device.default_output_config()?;
                let volume = volume.clone();
                // TODO: make this a macro
                match (config.sample_format(), config.channels()) {
                    (SampleFormat::F32, 1) => {
                        self.build_stream_with_conversion::<T, f32, 1>(source, config, volume)?
                    }
                    (SampleFormat::F32, _) => {
                        self.build_stream_with_conversion::<T, f32, 2>(source, config, volume)?
                    }
                    (SampleFormat::U16, 1) => {
                        self.build_stream_with_conversion::<T, u16, 1>(source, config, volume)?
                    }
                    (SampleFormat::U16, _) => {
                        self.build_stream_with_conversion::<T, u16, 2>(source, config, volume)?
                    }
                    (SampleFormat::I16, 1) => {
                        self.build_stream_with_conversion::<T, i16, 1>(source, config, volume)?
                    }
                    (SampleFormat::I16, _) => {
                        self.build_stream_with_conversion::<T, i16, 2>(source, config, volume)?
                    }
                }
            }
        };

        for event in self.event_receiver.iter() {
            match event {
                InternalEvent::Close => {
                    log::debug!("closing audio output");
                    break;
                }
                InternalEvent::Pause => {
                    log::debug!("pausing audio output");
                    if let Err(err) = stream.pause() {
                        log::error!("failed to stop device: {}", err);
                    }
                }
                InternalEvent::Resume => {
                    log::debug!("resuming audio output");
                    if let Err(err) = stream.play() {
                        log::error!("failed to start device: {}", err);
                    }
                }
                InternalEvent::SetVolume(vol) => {
                    log::debug!("volume has changed");
                    *volume.lock().unwrap() = vol as f32;
                }
            }
        }

        Ok(())
    }

    // F is the sample format and C the number of channels to use for the cpal audio sink.
    fn build_stream_with_conversion<T, F, const C: usize>(
        &self,
        source: Arc<Mutex<T>>,
        config: SupportedStreamConfig,
        volume: Arc<Mutex<f32>>,
    ) -> Result<Stream, Error>
    where
        T: AudioSource + Send + 'static,
        F: Sample + FromSample<f32> + cpal::Sample + 'static,
        [f32; C]: Frame<Sample = f32>,
    {
        let default_sample_rate = config.sample_rate();

        // assumed to be constant
        let source_sample_rate = source
            .lock()
            .expect("Failed to acquire audio source lock")
            .sample_rate();

        // create a signal that dasp can handle
        let source_signal = signal::from_interleaved_samples_iter::<_, [f32; C]>(
            // iterate over chunks of frames from the audio source and buffer them to prevent
            // excessive calls of lock
            std::iter::from_fn(move || {
                let mut source = source.lock().expect("Failed to acquire audio source lock");
                let channels_source = source.channels();
                let norm_factor = source.normalization_factor().unwrap_or(1.0);
                let volume = *volume.lock().unwrap();
                let mut buf = Vec::new();
                const N: usize = 1024;

                // Try to read N audio frames at a time from source.
                buf.reserve(N * C);
                for _ in 0..N {
                    for c in 0..C {
                        // make sure to only copy samples from channels that actually exist in
                        // source
                        if c < channels_source as usize {
                            if let Some(s) = source.next() {
                                buf.push(s * norm_factor * volume)
                            } else {
                                // this assumes that the source always contains whole frames
                                break;
                            }
                        } else {
                            // if the source has less channels than the audio sink, send silence to
                            // the additional channels
                            buf.push(0.0f32)
                        }
                    }
                    // if the source has more channels than the audio sink, drop all additional
                    // samples, so we do not get out of sync
                    for _ in C..channels_source as usize {
                        source.next();
                    }
                }
                // no samples -> send some silence, just enough to prevent this loop from getting
                // too hot
                if buf.is_empty() {
                    for _ in 0..N * C {
                        buf.push(0.0f32);
                    }
                }
                Some(buf)
            })
            // actually we only want single samples, so flatten the iterator of buffers containing
            // frames back to an iterator over samples.
            .flatten(),
        );
        // linear interpolations as this is fast, Sinc for example had my CPU at >40%, which is
        // inacceptable
        let interp = Linear::new([0.0f32; C], [0.0f32; C]);
        let mut resampled_signal = source_signal
            .from_hz_to_hz(
                interp,
                source_sample_rate as f64,
                default_sample_rate.0 as f64,
            )
            .until_exhausted()
            // flatten the iterator over frames to samples
            .flatten()
            // make sure we have the correct sample format
            .map(f32::to_sample::<F>);

        Ok(self.device.build_output_stream(
            &config.into(),
            move |output: &mut [F], _: &cpal::OutputCallbackInfo| {
                for sample in output.iter_mut() {
                    // calling unwrap is fine as source_signal is constructed to never end
                    let s = resampled_signal.next().unwrap();
                    *sample = s;
                }
            },
            |err| log::error!("an error occurred on stream: {}", err),
        )?)
    }
}

enum InternalEvent {
    Close,
    Pause,
    Resume,
    SetVolume(f64),
}

#[derive(Debug, Clone)]
struct CPalError(String);

impl std::fmt::Display for CPalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "cpal error: {}", self.0)
    }
}

impl std::error::Error for CPalError {}

impl From<cpal::DefaultStreamConfigError> for Error {
    fn from(err: cpal::DefaultStreamConfigError) -> Error {
        Error::AudioOutputError(Box::new(err))
    }
}

impl From<cpal::BuildStreamError> for Error {
    fn from(err: cpal::BuildStreamError) -> Error {
        Error::AudioOutputError(Box::new(err))
    }
}
