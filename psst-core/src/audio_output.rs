use std::sync::{Arc, Mutex};

use crossbeam_channel::{unbounded, Receiver, Sender};
use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    Device, SampleRate, StreamConfig,
};

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
            let default_config: StreamConfig = self.device.default_output_config()?.into();
            config = StreamConfig {
                channels: source.channels().into(),
                sample_rate: SampleRate(source.sample_rate()),
                buffer_size: default_config.buffer_size,
            };
        }

        let volume = Arc::new(Mutex::new(1.0f32));
        let channels = config.channels;
        // Move the source and a clone of the volume into the data callback.
        let stream = {
            let volume = volume.clone();
            self.device.build_output_stream(
                &config,
                move |output: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    let mut source = source.lock().expect("Failed to acquire audio source lock");
                    // Get the audio normalization factor.
                    let norm_factor = source.normalization_factor().unwrap_or(1.0);
                    let volume = *volume.lock().unwrap();
                    // Fill the buffer with audio samples from the source.
                    for frame in output.chunks_mut(channels as usize) {
                        for sample in frame.iter_mut() {
                            let s = source.next().unwrap_or(0.0); // Use silence in case the
                                                                  // source has finished.
                            *sample = s * norm_factor * volume;
                        }
                    }
                },
                |err| log::error!("an error occurred on stream: {}", err),
            )?
        };
        if let Err(err) = stream.play() {
            log::error!("failed to start stream: {}", err);
        }
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
