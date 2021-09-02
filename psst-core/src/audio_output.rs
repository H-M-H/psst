use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossbeam_channel::{unbounded, Receiver, Sender};
use rodio::{
    source::{SineWave, Source},
    OutputStream, Sink,
};

use crate::error::Error;

pub type AudioSample = f32;

pub trait AudioSource: Iterator<Item = AudioSample> {
    fn channels(&self) -> u8;
    fn sample_rate(&self) -> u32;
    fn normalization_factor(&self) -> Option<f32>;
}

/// Wrapper for AudioSource that buffers the provided audio to prevent execessive calls to lock of
/// the Mutex AudioSource is wrapped into.
struct AudioSourceBuffered<T: AudioSource> {
    source: Arc<Mutex<T>>,
    itr: std::vec::IntoIter<f32>,
}

impl<T: AudioSource> AudioSourceBuffered<T> {
    fn new(source: Arc<Mutex<T>>) -> Self {
        Self {
            source,
            itr: vec![].into_iter(),
        }
    }
}

impl<T: AudioSource> Iterator for AudioSourceBuffered<T> {
    type Item = f32;
    fn next(&mut self) -> Option<Self::Item> {
        match self.itr.next() {
            // if there is something inside the buffer use that
            s @ Some(_) => s,
            // otherwise refill the buffer
            None => {
                let mut source = self
                    .source
                    .lock()
                    .expect("Failed to acquire audio source lock");
                const N: usize = 2048;
                let mut buf = Vec::new();
                let channels = source.channels() as usize;
                let norm_factor = source.normalization_factor().unwrap_or(1.0);
                // Take N frames out of source and buffer them.
                buf.reserve(N * channels);
                for _ in 0..N * channels {
                    if let Some(s) = source.next() {
                        buf.push(s * norm_factor);
                    } else {
                        break;
                    }
                }
                // If source is empty, add some silence.
                if buf.is_empty() {
                    for _ in 0..N * channels {
                        buf.push(0.0f32);
                    }
                }
                self.itr = buf.into_iter();
                self.itr.next()
            }
        }
    }
}

impl<T: AudioSource> Source for AudioSourceBuffered<T> {
    fn total_duration(&self) -> Option<Duration> {
        None
    }

    fn sample_rate(&self) -> u32 {
        self.source
            .lock()
            .expect("Failed to acquire audio source lock")
            .sample_rate()
    }

    fn channels(&self) -> u16 {
        self.source
            .lock()
            .expect("Failed to acquire audio source lock")
            .channels() as u16
    }

    fn current_frame_len(&self) -> Option<usize> {
        if self.itr.len() > 0 {
            Some(self.itr.len())
        } else {
            None
        }
    }
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
    event_sender: Sender<InternalEvent>,
    event_receiver: Receiver<InternalEvent>,
}

impl AudioOutput {
    pub fn open() -> Result<Self, Error> {
        // Channel used for controlling the audio output.
        let (event_sender, event_receiver) = unbounded();

        Ok(Self {
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
        let (_stream, stream_handle) = OutputStream::try_default()?;
        let sink = Sink::try_new(&stream_handle)?;
        sink.append(AudioSourceBuffered::new(source));

        for event in self.event_receiver.iter() {
            match event {
                InternalEvent::Close => {
                    log::debug!("closing audio output");
                    sink.stop();
                    break;
                }
                InternalEvent::Pause => {
                    log::debug!("pausing audio output");
                    sink.pause();
                }
                InternalEvent::Resume => {
                    log::debug!("resuming audio output");
                    sink.play();
                }
                InternalEvent::SetVolume(volume) => {
                    log::debug!("volume has changed");
                    sink.set_volume(volume as f32);
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

impl From<rodio::StreamError> for Error {
    fn from(err: rodio::StreamError) -> Error {
        Error::AudioOutputError(Box::new(err))
    }
}

impl From<rodio::PlayError> for Error {
    fn from(err: rodio::PlayError) -> Error {
        Error::AudioOutputError(Box::new(err))
    }
}
