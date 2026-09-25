use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::Serialize;
use windows::core::Interface;
use windows::Media::Control::GlobalSystemMediaTransportControlsSessionManager as SessionManager;
use windows::Storage::Streams::{DataReader, IInputStream, IRandomAccessStreamReference};

#[derive(Clone, PartialEq, Debug, Serialize, Default)]
pub struct Track {
    pub title: String,
    pub artist: String,
    pub art: Option<String>,
}

fn art_data_uri(thumbnail: IRandomAccessStreamReference) -> windows::core::Result<String> {
    let stream = thumbnail.OpenReadAsync()?.get()?;
    let mime = stream.ContentType()?.to_string();
    let size = stream.Size()? as usize;
    let input: IInputStream = stream.cast()?;
    let reader = DataReader::CreateDataReader(&input)?;
    reader.LoadAsync(size as u32)?.get()?;
    let mut bytes = vec![0u8; size];
    reader.ReadBytes(&mut bytes)?;
    Ok(format!("data:{mime};base64,{}", STANDARD.encode(bytes)))
}

fn read() -> windows::core::Result<Option<Track>> {
    let manager = SessionManager::RequestAsync()?.get()?;
    for session in manager.GetSessions()? {
        let id = session.SourceAppUserModelId()?.to_string().to_lowercase();
        if !id.contains("spotify") {
            continue;
        }
        let props = session.TryGetMediaPropertiesAsync()?.get()?;
        let thumbnail = props.Thumbnail()?;
        let art = if Interface::as_raw(&thumbnail).is_null() { None } else { art_data_uri(thumbnail).ok() };
        return Ok(Some(Track { title: props.Title()?.to_string(), artist: props.Artist()?.to_string(), art }));
    }
    Ok(None)
}

pub fn current() -> Option<Track> {
    read().ok().flatten()
}
