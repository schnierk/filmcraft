//! The AI Director end to end, with movies made by our own encoders (no ffmpeg, no GPU).

use std::path::PathBuf;

use filmcraft_media::DemoScene;
use filmcraft_project::{ItemId, ItemKind, TrackKind};
use serde_json::json;

use super::assemble::{Medium, assemble, folder_title, timestamp};
use super::types::{Candidate, Plan};
use crate::Session;
use crate::media_test_util::{make_movie, tmp_dir};

/// Three short movies: two landscape on one day, one portrait three weeks later.
fn footage(name: &str) -> (PathBuf, Vec<(PathBuf, u32, u32, &'static str)>) {
    let dir = tmp_dir(name);
    let trip_a = dir.join("2026-09 Mykonos");
    let trip_b = dir.join("2026-10 Paris");
    std::fs::create_dir_all(&trip_a).unwrap();
    std::fs::create_dir_all(&trip_b).unwrap();
    let files = vec![
        (trip_a.join("beach.mp4"), 320, 180, "2026-09-05T10:00:00+03:00"),
        (trip_a.join("boat.mp4"), 320, 180, "2026-09-05T15:30:00+03:00"),
        (trip_b.join("tower.mp4"), 180, 320, "2026-10-01T12:00:00+02:00"),
    ];
    for (k, (p, w, h, _)) in files.iter().enumerate() {
        make_movie(p, DemoScene::ALL[k % DemoScene::ALL.len()], *w, *h, 72);
    }
    (dir, files)
}

fn plan(language: &str) -> Plan {
    Plan { language: language.into(), width: 640, height: 360, fps: 24.0, seconds: Some(9.0), ..Plan::default() }
}

#[test]
fn assemble_builds_one_undoable_film_with_chapters_blurred_portraits_and_titles() {
    let (_dir, files) = footage("regie-assemble");
    let mut s = Session::default();
    let paths: Vec<String> = files.iter().map(|f| f.0.to_string_lossy().into_owned()).collect();
    let r = s.execute("file.import", json!({"paths": paths})).unwrap();
    let items: Vec<u64> = r["items"].as_array().unwrap().iter().map(|v| v.as_u64().unwrap()).collect();
    assert_eq!(items.len(), 3, "{r}");
    let media: Vec<Medium> = files
        .iter()
        .zip(&items)
        .map(|((p, w, h, created), item)| Medium {
            candidate: Candidate {
                path: p.to_string_lossy().into_owned(),
                kind: "video".into(),
                created: Some(created.to_string()),
                width: *w,
                height: *h,
                seconds: Some(3.0),
                folder: p.parent().and_then(|d| d.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                item: Some(*item),
                ..Default::default()
            },
            highlights: None,
        })
        .collect();
    let undo_before = s.history.undo.len();
    let v = assemble(&mut s, &plan("de"), &media, None).unwrap();
    assert_eq!(s.history.undo.len(), undo_before + 1, "the whole film is one undo step");
    let seq = ItemId(v["sequence"].as_u64().unwrap());
    let q = s.project.sequence(seq).unwrap();
    assert_eq!((q.settings.width, q.settings.height), (640, 360));
    let main: Vec<_> = q.video_tracks.get(1).map(|t| t.items.clone()).unwrap_or_default();
    assert_eq!(main.len(), 3, "every medium on V2: {v}");
    // the portrait clip has a blurred, darkened copy below it on V1
    let bg = q.video_tracks.first().map(|t| t.items.clone()).unwrap_or_default();
    assert_eq!(bg.len(), 1, "one background copy, for the portrait clip");
    let effects: Vec<&str> = bg[0].effects.iter().map(|e| e.effect.as_str()).collect();
    assert!(effects.contains(&"gaussian_blur") && effects.contains(&"brightness_contrast"), "{effects:?}");
    // two chapters (Mykonos, then Paris three weeks later) and the film title on V3/V4
    let chapters = v["chapters"].as_array().unwrap();
    assert_eq!(chapters.len(), 2, "{v}");
    assert_eq!(chapters[0]["title"], "Mykonos");
    assert_eq!(chapters[0]["subtitle"], "September 2026");
    assert_eq!(chapters[1]["title"], "Paris");
    let titles = q.video_tracks.get(2).map(|t| t.items.len()).unwrap_or(0);
    assert!(titles >= 1, "titles on V3");
    // cuts are in time order without overlaps
    let mut ends = main.iter().map(|c| (c.start, c.start + c.duration)).collect::<Vec<_>>();
    ends.sort();
    assert!(ends.windows(2).all(|w| w[0].1 <= w[1].0), "{ends:?}");
    assert!(v["summary"][0].as_str().unwrap().contains("Kapitel"), "{v}");
    // undo removes the film
    s.execute("edit.undo", json!({})).unwrap();
    assert!(s.project.sequence(seq).is_none());
}

#[test]
fn run_finds_the_media_in_a_folder_and_builds_the_film() {
    let (dir, _files) = footage("regie-run");
    let mut s = Session::default();
    let r =
        s.execute("regie.run", json!({"request": "Mach einen Rückblick in 16:9, etwa 8 Sekunden", "folders": [dir.to_string_lossy()], "wait": true})).unwrap();
    let result = &r["result"];
    assert!(result["clips"].as_u64().unwrap() >= 2, "{r}");
    let seq = ItemId(result["sequence"].as_u64().unwrap());
    assert!(s.project.sequence(seq).is_some());
    // the media are in a new bin
    assert!(s.project.root.children.iter().any(|c| matches!(c, filmcraft_project::BinEntry::Bin(b) if !b.children.is_empty())), "media in a bin");
    let status = s.execute("regie.status", json!({})).unwrap();
    assert_eq!(status["running"], false);
    assert_eq!(status["result"]["sequence"], result["sequence"]);
    // one undo step removes the film and the imported media
    let media_before = s.project.items.values().filter(|i| matches!(i.kind, ItemKind::Media(_))).count();
    assert!(media_before >= 3);
    s.execute("edit.undo", json!({})).unwrap();
    assert!(s.project.sequence(seq).is_none());
    assert_eq!(s.project.items.values().filter(|i| matches!(i.kind, ItemKind::Media(_))).count(), 0);
    let _ = TrackKind::Video;
}

#[test]
fn run_without_matching_media_reports_it_and_changes_nothing() {
    let dir = tmp_dir("regie-empty");
    let mut s = Session::default();
    let e = s.execute("regie.run", json!({"request": "Urlaub 2026", "folders": [dir.to_string_lossy()], "wait": true})).unwrap_err();
    assert!(e.to_string().contains("keine passenden"), "{e}");
    assert!(s.project.items.values().all(|i| !matches!(i.kind, ItemKind::Media(_))));
    // hostile params
    assert!(s.execute("regie.run", json!({"plan": 5})).is_err());
    assert!(s.execute("regie.run", json!({})).is_err());
    assert!(s.execute("regie.assemble", json!({"plan": {"width": 0}, "media": []})).is_err());
}

#[test]
fn helpers() {
    assert_eq!(folder_title("2026-09 Mykonos"), "Mykonos");
    assert_eq!(folder_title("05_sommer"), "Sommer");
    assert_eq!(folder_title("barcelona_sitges"), "Barcelona Sitges");
    let a = timestamp("2026-09-05T10:00:00+03:00").unwrap();
    let b = timestamp("2026-09-05T07:00:00Z").unwrap();
    assert!((a - b).abs() < 1e-6, "offsets apply");
    assert!(timestamp("garbage").is_none());
    assert!(timestamp("2026-13-01").is_none());
}
