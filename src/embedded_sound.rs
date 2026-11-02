use std::path::Path;
use std::process::Command;

const FFPROBE_DEFAULT_DISPOSITION: &str = "1";

#[derive(Debug, PartialEq)]
pub struct EmbeddedSound {
    pub stream_indexes: Vec<usize>,
    pub channels: u16,
}

#[derive(Debug)]
struct AudioStream {
    index: usize,
    channels: u16,
    default: bool,
}

pub fn embedded_sound(picture: &Path) -> Result<Option<EmbeddedSound>, String> {
    let output = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "a"])
        .args([
            "-show_entries",
            "stream=index,channels:stream_disposition=default",
            "-of",
            "csv=p=0",
        ])
        .arg(picture)
        .output()
        .map_err(|e| format!("cannot run ffprobe: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "ffprobe cannot read the sound in {}: {}",
            picture.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let streams = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(audio_stream)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(sound_of_streams(&streams))
}

fn audio_stream(line: &str) -> Result<AudioStream, String> {
    let unreadable = || format!("ffprobe gave an audio stream line {line:?}");
    let mut fields = line.trim().split(',');
    let (Some(index), Some(channels), Some(default)) =
        (fields.next(), fields.next(), fields.next())
    else {
        return Err(unreadable());
    };
    Ok(AudioStream {
        index: index.parse().map_err(|_| unreadable())?,
        channels: channels.parse().map_err(|_| unreadable())?,
        default: default == FFPROBE_DEFAULT_DISPOSITION,
    })
}

fn sound_of_streams(streams: &[AudioStream]) -> Option<EmbeddedSound> {
    let all_mono = streams.iter().all(|stream| stream.channels == 1);
    if streams.len() > 1 && all_mono {
        return Some(EmbeddedSound {
            stream_indexes: streams.iter().map(|stream| stream.index).collect(),
            channels: streams.len() as u16,
        });
    }
    // ffmpeg with no map takes a default stream first, then the widest, then the first of equals
    let chosen = streams
        .iter()
        .rev()
        .max_by_key(|stream| (stream.default, stream.channels))?;
    Some(EmbeddedSound {
        stream_indexes: vec![chosen.index],
        channels: chosen.channels,
    })
}

impl EmbeddedSound {
    pub fn ffmpeg_arguments(&self, filters: &str) -> [String; 4] {
        let inputs: String = self
            .stream_indexes
            .iter()
            .map(|index| format!("[0:{index}]"))
            .collect();
        let chain = match self.stream_indexes.len() {
            1 => filters.to_string(),
            count => format!("amerge=inputs={count},{filters}"),
        };
        [
            "-filter_complex".into(),
            format!("{inputs}{chain}[sound]"),
            "-map".into(),
            "[sound]".into(),
        ]
    }
}

#[cfg(test)]
mod tests;
