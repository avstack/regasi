pub(crate) mod aws;

use anyhow::Result;
use gstreamer::Element;
use tokio::sync::mpsc;

use crate::model::{Participant, TranscriptionEvent};

pub(crate) trait Transcriber {
  fn new(
    region: Option<&str>,
    participant: &Participant,
    tx: mpsc::Sender<TranscriptionEvent>,
  ) -> Self
  where
    Self: Sized;
  fn make_element(&self) -> Result<Element>;
}
