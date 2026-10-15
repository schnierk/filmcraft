//! What the KI-Regie steps hand each other. Every type is plain data with a camelCase JSON form,
//! so agents can read and write it on the command surface, and every field has a default, so a
//! partial JSON object is accepted (missing fields take their defaults).

use serde::{Deserialize, Serialize};

/// What the user asked for, as `regie.plan` understood it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct Plan {
    /// The request as typed or spoken.
    pub request: String,
    /// Language of the request and of every text made for the user: "de" or "en".
    pub language: String,
    /// "recap" (a film of events: holidays, a year), "reel" (short, fast, often vertical),
    /// "slideshow" (calm, photos first) or "musicVideo" (cut to a song).
    pub kind: String,
    /// "16:9", "9:16", "1:1" or "4:5".
    pub aspect: String,
    /// Sequence frame size and rate.
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    /// Wanted length in seconds; `None`: from the music, else from the media.
    pub seconds: Option<f64>,
    /// Capture dates to use, inclusive, as `YYYY-MM-DD`; `None`: open-ended.
    pub from: Option<String>,
    pub to: Option<String>,
    /// Words a place name, a folder name or a file name must contain (any of them); empty: all.
    pub places: Vec<String>,
    /// "upbeat", "calm" or "epic": pacing, transitions and the music to look for.
    pub mood: String,
    /// The film's title; `None`: made from the content ("Mein Reisejahr 2026").
    pub title: Option<String>,
    /// The new sequence's name; `None`: the title.
    pub sequence_name: Option<String>,
    /// A music file to cut to; `None`: the first suitable audio file found, else no music.
    pub music: Option<String>,
    /// Which kinds of media to use.
    pub photos: bool,
    pub videos: bool,
    /// Folders to search; empty: the library folders from the preferences.
    pub folders: Vec<String>,
    /// What was understood, one line each, in `language`, for the panel to show.
    pub understood: Vec<String>,
    /// What is unclear, in `language` (the plan is still usable: defaults fill the gaps).
    pub questions: Vec<String>,
}

impl Default for Plan {
    fn default() -> Self {
        Plan {
            request: String::new(),
            language: "en".into(),
            kind: "recap".into(),
            aspect: "16:9".into(),
            width: 1920,
            height: 1080,
            fps: 25.0,
            seconds: None,
            from: None,
            to: None,
            places: Vec::new(),
            mood: "upbeat".into(),
            title: None,
            sequence_name: None,
            music: None,
            photos: true,
            videos: true,
            folders: Vec::new(),
            understood: Vec::new(),
            questions: Vec::new(),
        }
    }
}

/// One media file that may go into the film.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct Candidate {
    /// Absolute path.
    pub path: String,
    /// "video", "photo" or "audio".
    pub kind: String,
    /// When it was captured, RFC 3339 with the camera's UTC offset when known
    /// (`2026-09-05T18:22:00+03:00`), else without one; `None`: unknown (not the file's
    /// modification time, which `modified` holds).
    pub created: Option<String>,
    /// The file's modification time, RFC 3339 UTC.
    pub modified: Option<String>,
    /// Where it was captured: [latitude, longitude] in degrees.
    pub location: Option<[f64; 2]>,
    /// A place name, when a library provides one.
    pub place: Option<String>,
    /// Picture size as displayed (rotation applied); 0 when unknown.
    pub width: u32,
    pub height: u32,
    /// Length of a video or audio file in seconds.
    pub seconds: Option<f64>,
    /// Name of the folder it is in (often the event: "Mykonos 2026").
    pub folder: String,
    /// The project item, once imported.
    pub item: Option<u64>,
}

/// A stretch of a video worth using.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct Segment {
    /// Media time in seconds.
    pub start: f64,
    pub end: f64,
    /// 0..1, higher is better.
    pub score: f64,
    /// Why it scored so, short words in English ("sharp", "steady", "lively", "well exposed").
    pub reasons: Vec<String>,
}

/// The best moments of one video (`media.highlights`).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct Highlights {
    /// Length analysed, seconds.
    pub seconds: f64,
    /// Best non-overlapping segments, in media order.
    pub segments: Vec<Segment>,
    /// The score over time: [seconds, score] pairs.
    pub curve: Vec<[f64; 2]>,
}

/// A calm or lively stretch of music.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct Section {
    /// "calm" or "lively".
    pub kind: String,
    pub start: f64,
    pub end: f64,
}

/// The beat grid of a piece of music (`audio.beats`).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct Beats {
    /// Length, seconds.
    pub seconds: f64,
    /// Tempo, beats per minute.
    pub bpm: f64,
    /// Every beat, seconds from the start.
    pub beats: Vec<f64>,
    /// Bar starts (downbeats), seconds.
    pub bars: Vec<f64>,
    /// Loudness per beat, 0..1.
    pub energy: Vec<f64>,
    /// Calm and lively stretches, in order, covering the music.
    pub sections: Vec<Section>,
}
