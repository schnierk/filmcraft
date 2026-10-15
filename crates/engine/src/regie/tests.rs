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
            background: None,
            background_item: None,
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
fn variant_cuts_a_reel_from_the_last_film_without_searching_again() {
    let (dir, _files) = footage("regie-variant");
    let mut s = Session::default();
    // no film yet: nothing to cut a version from
    let e = s.execute("regie.variant", json!({})).unwrap_err().to_string();
    assert!(e.contains("no film"), "{e}");
    let r = s
        .execute(
            "regie.run",
            json!({"request": "Mach einen Rückblick in 16:9, etwa 8 Sekunden, Titel \"Herbst\"", "folders": [dir.to_string_lossy()], "wait": true}),
        )
        .unwrap();
    let film = ItemId(r["result"]["sequence"].as_u64().unwrap());
    let undo_before = s.history.undo.len();
    let v = s.execute("regie.variant", json!({"seconds": 6})).unwrap();
    assert_eq!(s.history.undo.len(), undo_before + 1, "the version is one undo step");
    let reel = ItemId(v["sequence"].as_u64().unwrap());
    assert_ne!(reel, film);
    let q = s.project.sequence(reel).unwrap();
    assert_eq!((q.settings.width, q.settings.height), (1080, 1920));
    assert_eq!(v["aspect"], "9:16");
    assert_eq!(v["title"], "Herbst", "the film's title, not a new one");
    let name = &s.project.item(reel).unwrap().name;
    assert_eq!(name, "Herbst (Reel 9:16)");
    assert!(v["clips"].as_u64().unwrap() >= 2, "{v}");
    assert!(v["seconds"].as_f64().unwrap() <= 6.5, "{v}");
    assert!(v["summary"][0].as_str().unwrap().contains("ohne neue Suche"), "{v}");
    // the film stays; regie.status reports the version
    assert!(s.project.sequence(film).is_some());
    assert_eq!(s.execute("regie.status", json!({})).unwrap()["result"]["sequence"], v["sequence"]);
    // a square version of the same film (not of the reel)
    let sq = s.execute("regie.variant", json!({"aspect": "1:1", "seconds": 5})).unwrap();
    let q = s.project.sequence(ItemId(sq["sequence"].as_u64().unwrap())).unwrap();
    assert_eq!((q.settings.width, q.settings.height), (1080, 1080));
    assert_eq!(sq["title"], "Herbst");
    // hostile parameters are errors
    for bad in [json!({"aspect": "21:9"}), json!({"aspect": 5}), json!({"seconds": 0}), json!({"seconds": "lang"}), json!({"seconds": 1e9}), json!([1])] {
        assert!(s.execute("regie.variant", bad.clone()).is_err(), "{bad}");
    }
    // undoing the film takes its media out of the project: nothing is left to cut from
    while s.project.sequence(film).is_some() {
        s.execute("edit.undo", json!({})).unwrap();
    }
    assert!(s.execute("regie.variant", json!({})).is_err());
}

#[test]
fn the_closing_title_is_not_lost_to_a_late_chapter_name() {
    // six stills of two seconds: four on Mykonos, then two in Paris three weeks later. The Paris
    // chapter starts where the closing title goes (the last four seconds); its name used to take
    // that place on the same track, and the closing title was dropped.
    let (_dir, files) = footage("regie-outro");
    let mut s = Session::default();
    let paths: Vec<String> = files.iter().map(|f| f.0.to_string_lossy().into_owned()).collect();
    let r = s.execute("file.import", json!({"paths": paths})).unwrap();
    let item = r["items"][0].as_u64().unwrap();
    let still = |k: usize, folder: &str, created: &str| Medium {
        candidate: Candidate {
            path: format!("/photos/{folder}/IMG_{k}.jpg"),
            kind: "photo".into(),
            created: Some(created.into()),
            width: 320,
            height: 180,
            folder: folder.into(),
            item: Some(item),
            ..Default::default()
        },
        ..Default::default()
    };
    let media = vec![
        still(1, "Mykonos", "2026-09-05T10:00:00+03:00"),
        still(2, "Mykonos", "2026-09-05T11:00:00+03:00"),
        still(3, "Mykonos", "2026-09-05T12:00:00+03:00"),
        still(4, "Mykonos", "2026-09-05T13:00:00+03:00"),
        still(5, "Paris", "2026-10-01T12:00:00+02:00"),
        still(6, "Paris", "2026-10-01T13:00:00+02:00"),
    ];
    let v = assemble(&mut s, &Plan { seconds: Some(12.0), ..plan("de") }, &media, None).unwrap();
    let end = v["seconds"].as_f64().unwrap();
    assert_eq!(v["chapters"].as_array().unwrap().len(), 2, "{v}");
    assert!(end > 8.0, "{v}");
    let q = s.project.sequence(ItemId(v["sequence"].as_u64().unwrap())).unwrap();
    let secs = |t: filmcraft_time::Tick| t.0 as f64 / filmcraft_time::TICKS_PER_SECOND as f64;
    let titles = q.video_tracks.get(2).map(|t| t.items.clone()).unwrap_or_default();
    let starts: Vec<f64> = titles.iter().map(|c| secs(c.start)).collect();
    assert!(starts.iter().any(|t| (t - (end - 4.0)).abs() < 0.05), "a closing title at {}: {starts:?}", end - 4.0);
    // nothing before it on that track runs into it
    assert!(titles.iter().filter(|c| secs(c.start) < end - 4.05).all(|c| secs(c.start + c.duration) <= end - 4.0 + 0.05), "{starts:?}");
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
fn backgrounds_shrink_by_averaging_whole_blocks() {
    use super::run::shrink;
    // 5 × 3 pixels, longer side to 2: blocks of 3 (the last column and row are smaller)
    let px: Vec<u8> = (0..15u8).flat_map(|i| [i * 10, 0, 255 - i * 10, 255]).collect();
    let (out, w, h) = shrink(&px, 5, 3, 2).unwrap();
    assert_eq!((w, h), (2, 1));
    // first block: pixels 0, 1, 2, 5, 6, 7, 10, 11, 12 → red average 60
    assert_eq!(&out[0..4], &[60, 0, 195, 255]);
    // second block: pixels 3, 4, 8, 9, 13, 14 → red average 85
    assert_eq!(&out[4..8], &[85, 0, 170, 255]);
    // already small enough, or a buffer that does not match its size
    assert_eq!(shrink(&px, 5, 3, 480).unwrap(), (px.clone(), 5, 3));
    assert!(shrink(&px, 6, 3, 2).is_none());
    // a 4000 × 3000 photo comes out at 480 × 360 or less
    let big = vec![128u8; 4000 * 3000 * 4];
    let (_, w, h) = shrink(&big, 4000, 3000, 480).unwrap();
    assert!(w <= 480 && h <= 480 && w >= 440, "{w}×{h}");
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

/// A portrait film sizes its titles by the frame's short side, so they fit across it.
#[test]
fn portrait_titles_fit_the_width() {
    let (_dir, files) = footage("regie-portrait");
    let mut s = Session::default();
    let paths: Vec<String> = files.iter().map(|f| f.0.to_string_lossy().into_owned()).collect();
    let r = s.execute("file.import", json!({"paths": paths})).unwrap();
    let items: Vec<u64> = r["items"].as_array().unwrap().iter().map(|v| v.as_u64().unwrap()).collect();
    let media: Vec<Medium> = files
        .iter()
        .zip(&items)
        .map(|((_, w, h, created), item)| Medium {
            candidate: Candidate {
                kind: "video".into(),
                created: Some(created.to_string()),
                width: *w,
                height: *h,
                seconds: Some(3.0),
                item: Some(*item),
                ..Default::default()
            },
            highlights: None,
            background: None,
            background_item: None,
        })
        .collect();
    let plan = Plan { language: "de".into(), aspect: "9:16".into(), width: 360, height: 640, fps: 24.0, seconds: Some(9.0), ..Plan::default() };
    let v = assemble(&mut s, &plan, &media, None).unwrap();
    let q = s.project.sequence(ItemId(v["sequence"].as_u64().unwrap())).unwrap();
    let sizes: Vec<f64> = q
        .video_tracks
        .iter()
        .flat_map(|t| t.items.iter())
        .flat_map(|c| c.effects.iter())
        .filter(|e| e.effect == "graphic_text")
        .filter_map(|e| {
            e.params.get("size").and_then(|p| match &p.value {
                filmcraft_project::ParamValue::Float(f) => Some(*f),
                _ => None,
            })
        })
        .collect();
    assert!(!sizes.is_empty(), "titles were made");
    assert!(sizes.iter().all(|s| *s <= 360.0 * 0.09), "sized by the 360 px short side: {sizes:?}");
}
