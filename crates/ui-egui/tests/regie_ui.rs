//! Headless UI tests of the AI Director panel (Window ▸ AI Director, KI-Regie): it opens from the
//! Window menu with every control, the example chips fill the wish, Understand shows the plan
//! (`regie.plan`) and its fields change it, the folder list and the panel state go through the
//! control channel, the panel speaks German, and Make film, a lost job and hostile state end in a
//! friendly line instead of a crash.
//!
//! Set `FILMCRAFT_UI_SNAPSHOT_DIR=<dir>` to also render offscreen with wgpu and write `regie-*.png`.

use std::sync::mpsc::{Sender, channel};

use egui_kittest::Harness;
use filmcraft_engine::Session;
use filmcraft_ui_egui::FilmcraftApp;
use filmcraft_ui_egui::control::ControlRequest;
use serde_json::{Value, json};

struct Driver {
    harness: Harness<'static, FilmcraftApp>,
    tx: Sender<ControlRequest>,
    snapshots: Option<std::path::PathBuf>,
}

impl Driver {
    fn demo() -> Self {
        let mut session = Session::default();
        session.execute("file.openDemoProject", json!({})).expect("demo project");
        let (tx, rx) = channel();
        let app = FilmcraftApp::new(session).with_control(rx);
        let snapshots = std::env::var_os("FILMCRAFT_UI_SNAPSHOT_DIR").map(std::path::PathBuf::from);
        let mut b = Harness::builder().with_size(egui::vec2(1600.0, 980.0)).with_max_steps(10_000);
        if snapshots.is_some() {
            b = b.wgpu();
        }
        let harness = b.build_eframe(move |_cc| app);
        let mut d = Driver { harness, tx, snapshots };
        d.frames(4);
        d
    }

    /// Offscreen render of the panel (only with `FILMCRAFT_UI_SNAPSHOT_DIR`).
    fn snapshot(&mut self, name: &str) {
        let Some(dir) = self.snapshots.clone() else { return };
        let rect = self.element("panel.Regie").map(|e| e["rect"].clone());
        self.frames(2);
        let img = match self.harness.render() {
            Ok(i) => i,
            Err(e) => {
                eprintln!("snapshot {name} skipped: {e}");
                return;
            }
        };
        let ppp = img.width() as f32 / 1600.0;
        let img = match rect {
            Some(r) => {
                let f = |i: usize| r[i].as_f64().unwrap_or(0.0) as f32 * ppp;
                let (x, y) = ((f(0) - 4.0).max(0.0) as u32, (f(1) - 34.0).max(0.0) as u32);
                let (w, h) = (((f(2) + 8.0) as u32).min(img.width() - x), ((f(3) + 38.0) as u32).min(img.height() - y));
                image::imageops::crop_imm(&img, x, y, w, h).to_image()
            }
            None => img,
        };
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("regie-{name}.png"));
        img.save(&path).unwrap();
        eprintln!("snapshot: {}", path.display());
    }

    fn frames(&mut self, n: usize) {
        for _ in 0..n {
            let ctx = self.harness.ctx.clone();
            let mut raw = std::mem::take(self.harness.input_mut());
            eframe::App::raw_input_hook(self.harness.state_mut(), &ctx, &mut raw);
            *self.harness.input_mut() = raw;
            self.harness.step();
        }
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        let (req, reply) = ControlRequest::new(method, params.clone());
        self.tx.send(req).unwrap();
        for _ in 0..600 {
            self.frames(1);
            if let Ok(v) = reply.try_recv() {
                return v;
            }
        }
        panic!("no reply to {method} {params}");
    }

    fn ok(&mut self, method: &str, params: Value) -> Value {
        let v = self.call(method, params.clone());
        assert_eq!(v["ok"], json!(true), "{method} {params} failed: {v}");
        v["result"].clone()
    }

    fn click(&mut self, id: &str) {
        self.ok("ui.click", json!({"id": id}));
        self.frames(3);
    }

    fn type_text(&mut self, text: &str) {
        self.ok("ui.type", json!({"text": text}));
        self.frames(3);
    }

    /// The panel state (`ui.inspect` → `ui.regie`).
    fn regie(&mut self) -> Value {
        self.ok("ui.inspect", json!({}))["ui"]["regie"].clone()
    }

    fn elements(&mut self, prefix: &str) -> Vec<Value> {
        self.ok("ui.elements", json!({"prefix": prefix})).as_array().cloned().unwrap_or_default()
    }

    fn ids(&mut self, prefix: &str) -> Vec<String> {
        self.elements(prefix).iter().filter_map(|e| e["id"].as_str().map(str::to_string)).collect()
    }

    fn element(&mut self, id: &str) -> Option<Value> {
        self.elements(id).into_iter().find(|e| e["id"] == json!(id))
    }

    fn has(&mut self, id: &str) -> bool {
        self.element(id).is_some()
    }

    fn label(&mut self, id: &str) -> String {
        self.element(id).and_then(|e| e["label"].as_str().map(str::to_string)).unwrap_or_default()
    }

    /// Window ▸ AI Director.
    fn open(&mut self) {
        self.ok("ui.menu.invoke", json!({"id": "window.panel.Regie"}));
        self.frames(3);
    }

    /// Step frames (real time passes: the panel polls every 250 ms) until the film's job is over.
    fn until_idle(&mut self, seconds: f64) {
        let end = std::time::Instant::now() + std::time::Duration::from_secs_f64(seconds);
        while std::time::Instant::now() < end {
            if self.regie()["job"].is_null() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
            self.frames(1);
        }
        panic!("the film's job never ended: {}", self.regie());
    }
}

fn scratch(name: &str) -> String {
    let d = std::env::temp_dir().join(format!("filmcraft-regie-ui-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d.to_string_lossy().into_owned()
}

#[test]
fn opens_from_the_window_menu_with_every_control() {
    let mut d = Driver::demo();
    let menu = d.ok("ui.menu.list", json!({}));
    let entry = menu.as_array().unwrap().iter().find(|m| m["id"] == "window.panel.Regie").cloned();
    assert_eq!(entry.as_ref().map(|m| (m["label"].clone(), m["path"].clone())), Some((json!("AI Director"), json!(["Window"]))), "{entry:?}");
    d.open();
    assert!(d.has("panel.tab.Regie"), "no tab");
    assert_eq!(d.label("panel.tab.Regie"), "AI Director");
    assert_eq!(d.ok("ui.inspect", json!({}))["ui"]["focused"], json!("Regie"));
    // a side panel beside Properties: the Program monitor stays in view
    assert!(!d.ids("program.").is_empty(), "Program monitor still shown");
    let ids = d.ids("regie.");
    for id in [
        "regie.request",
        "regie.example.0",
        "regie.example.1",
        "regie.example.2",
        "regie.example.3",
        "regie.section.folders",
        "regie.folders.default",
        "regie.folder.add",
        "regie.understand",
        "regie.make",
        "regie.section.plan",
    ] {
        assert!(ids.iter().any(|i| i == id), "missing {id} in {ids:?}");
    }
    assert_eq!(d.label("regie.folders.default"), "Pictures, Videos, Music (the default folders)");
    // nothing typed yet: nothing to understand or make
    assert_eq!(d.element("regie.understand").unwrap()["enabled"], json!(false));
    assert_eq!(d.element("regie.make").unwrap()["enabled"], json!(false));
    let r = d.regie();
    assert!(r["plan"].is_null() && r["job"].is_null() && r["result"].is_null(), "{r}");
    assert_eq!(r["error"], "");
    d.snapshot("empty");
}

#[test]
fn example_chips_fill_the_wish() {
    let mut d = Driver::demo();
    d.open();
    d.click("regie.example.1");
    assert_eq!(d.regie()["request"], "Instagram reel 9:16, 30 seconds, fast");
    assert_eq!(d.label("regie.example.1"), "Instagram reel 9:16, 30 seconds, fast");
    d.click("regie.example.3");
    assert_eq!(d.regie()["request"], "Music video cut to my song");
    assert_eq!(d.label("regie.request"), "Music video cut to my song");
    assert_eq!(d.element("regie.understand").unwrap()["enabled"], json!(true));
}

#[test]
fn understand_shows_the_plan_and_its_fields_change_it() {
    let mut d = Driver::demo();
    d.open();
    // room for every field: Maximize Frame on the panel
    d.click("panel.menu.Regie");
    d.click("panel.menu.Regie.maximize");
    d.click("regie.request");
    d.type_text("Holiday recap, upbeat");
    assert_eq!(d.regie()["request"], "Holiday recap, upbeat");
    d.click("regie.understand");
    let r = d.regie();
    assert_eq!(r["error"], "", "{r}");
    let plan = &r["plan"];
    // the engine reads the wish by its rules (phones film at 30 frames per second)
    assert_eq!(plan["request"], "Holiday recap, upbeat");
    assert_eq!(plan["aspect"], "16:9");
    d.frames(2);
    assert!(d.label("regie.aspect").starts_with("16:9"), "{}", d.label("regie.aspect"));
    assert_eq!(d.label("regie.resolution"), "HD 1920×1080");
    assert_eq!(d.label("regie.fps"), "30 fps");
    assert_eq!(d.label("regie.mood"), "Upbeat");
    d.snapshot("plan");
    // change the key fields
    d.click("regie.aspect");
    d.click("regie.aspect.9x16");
    let plan = d.regie()["plan"].clone();
    assert_eq!((plan["aspect"].clone(), plan["width"].clone(), plan["height"].clone()), (json!("9:16"), json!(1080), json!(1920)));
    d.click("regie.resolution");
    d.click("regie.resolution.uhd");
    let plan = d.regie()["plan"].clone();
    assert_eq!((plan["width"].clone(), plan["height"].clone()), (json!(2160), json!(3840)));
    assert_eq!(d.label("regie.resolution"), "4K 2160×3840");
    d.click("regie.fps");
    d.click("regie.fps.50");
    assert_eq!(d.regie()["plan"]["fps"], json!(50.0));
    d.click("regie.mood");
    d.click("regie.mood.calm");
    assert_eq!(d.regie()["plan"]["mood"], "calm");
    d.click("regie.from");
    d.type_text("2026-06-01");
    d.click("regie.title");
    d.type_text("Sommer 2026");
    let plan = d.regie()["plan"].clone();
    assert_eq!((plan["from"].clone(), plan["title"].clone()), (json!("2026-06-01"), json!("Sommer 2026")));
    // the length comes back in the field (0 = automatic)
    assert_eq!(d.label("regie.seconds"), "0");
    d.ok("ui.set", json!({"regie": {"plan": {"seconds": 180}}}));
    d.frames(2);
    assert_eq!(d.label("regie.seconds"), "180");
    // a changed wish leaves the plan, marked as out of date, until Understand runs again
    d.click("regie.request");
    d.ok("ui.key", json!({"key": "End"}));
    d.type_text(" with music");
    let r = d.regie();
    assert_eq!(r["plan"]["mood"], "calm", "kept: {r}");
    d.click("regie.understand");
    assert_eq!(d.regie()["plan"]["request"], "Holiday recap, upbeat with music");
}

#[test]
fn make_film_ends_in_a_friendly_line_not_a_crash() {
    let dir = scratch("make");
    let mut d = Driver::demo();
    d.open();
    // only an empty folder to search, so no real director scans this machine's media
    d.ok("ui.set", json!({"regie": {"folders": [dir.clone()]}}));
    d.click("regie.request");
    d.type_text("Birthday slideshow with music");
    d.click("regie.make");
    if filmcraft_engine::find_command("regie.run").is_none() {
        // the engine stub has no `regie.run` yet
        let r = d.regie();
        assert_eq!(r["error"], "The AI Director can't do this yet in this version of FilmCraft.", "{r}");
        assert!(r["job"].is_null());
        d.frames(2);
        assert_eq!(d.label("regie.error"), "The AI Director can't do this yet in this version of FilmCraft.");
        d.snapshot("error");
    } else {
        // the real director: an empty folder makes no film (or an empty one); either way it ends
        d.until_idle(30.0);
        let r = d.regie();
        assert!(r["error"] != json!("") || !r["result"].is_null(), "{r}");
    }
    // the panel goes on working
    assert_eq!(d.element("regie.make").unwrap()["enabled"], json!(true));
    d.click("regie.understand");
    assert_eq!(d.regie()["plan"]["request"], "Birthday slideshow with music");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_lost_job_ends_with_a_friendly_line_and_cancel_stops_it() {
    let mut d = Driver::demo();
    d.open();
    // a job the session doesn't know (an agent's typo, a job from another session)
    d.ok("ui.set", json!({"regie": {"job": 987_654_321u64, "status": "Finding media"}}));
    d.frames(2);
    assert!(d.has("regie.cancel"), "Cancel while the film is being made");
    assert_eq!(d.label("regie.progress"), "Finding media");
    assert_eq!(d.element("regie.make").unwrap()["enabled"], json!(false), "no second film while one runs");
    d.snapshot("running");
    d.click("regie.cancel");
    let r = d.regie();
    assert!(r["job"].is_null(), "{r}");
    assert_eq!(r["error"], "Stopped. No film was made.");
    // without Cancel the panel stops waiting by itself
    d.ok("ui.set", json!({"regie": {"job": 987_654_322u64}}));
    d.until_idle(10.0);
    assert_eq!(d.regie()["error"], "The AI Director stopped without making a film.");
    assert!(!d.has("regie.cancel"));
}

#[test]
fn folders_and_state_go_through_the_control_channel() {
    let mut d = Driver::demo();
    d.open();
    d.ok("ui.set", json!({"regie": {"folders": ["/media/holiday", "/media/holiday", "  ", "/media/music"]}}));
    assert_eq!(d.regie()["folders"], json!(["/media/holiday", "/media/music"]));
    d.frames(2);
    assert_eq!(d.label("regie.folder.1"), "/media/music");
    assert!(!d.has("regie.folders.default"));
    d.click("regie.folder.remove.0");
    assert_eq!(d.regie()["folders"], json!(["/media/music"]));
    d.click("regie.folder.remove.0");
    assert_eq!(d.regie()["folders"], json!([]));
    assert!(d.has("regie.folders.default"));
    // Add folder… needs a folder picker; the headless host has none
    assert_eq!(d.element("regie.folder.add").unwrap()["enabled"], json!(false));
    // the sections fold away and come back
    d.click("regie.section.folders");
    assert!(!d.has("regie.folder.add"));
    assert_eq!(d.regie()["openSections"], json!(["plan"]));
    d.click("regie.section.folders");
    assert!(d.has("regie.folder.add"));
    // a whole state at once, and hostile values: refused or kept in bounds, never a crash
    d.ok("ui.set", json!({"regie": {"request": "Reel", "plan": {"request": "Reel", "aspect": "21:9", "fps": -5, "width": 0, "seconds": -3, "understood": ["A reel"], "questions": ["Which song?"]}}}));
    d.ok("ui.set", json!({"regie": {"progress": 99.0}}));
    d.frames(3);
    assert_eq!(d.regie()["progress"], json!(1.0));
    assert_eq!(d.label("regie.fps"), "–");
    assert_eq!(d.label("regie.seconds"), "0");
    assert!(d.label("regie.aspect").starts_with("21:9"));
    let v = d.call("ui.set", json!({"regie": {"folders": "not a list"}}));
    assert_eq!(v["ok"], json!(false), "{v}");
    let v = d.call("ui.set", json!({"regie": {"plan": {"width": -1}}}));
    assert_eq!(v["ok"], json!(false), "{v}");
    assert_eq!(d.regie()["request"], "Reel", "a refused patch changes nothing");
    // a result that names no sequence: nothing to play or undo, but it shows and explains
    d.ok("ui.set", json!({"regie": {"result": {"clips": 3, "seconds": 75, "summary": ["Three clips"], "explain": ["File ▸ Import…"]}}}));
    d.frames(2);
    assert_eq!(d.element("regie.play").unwrap()["enabled"], json!(false));
    assert_eq!(d.element("regie.undo").unwrap()["enabled"], json!(false));
    d.click("regie.explain");
    assert!(d.regie()["openSections"].as_array().unwrap().iter().any(|s| s == "explain"));
    assert!(d.label("regie.result").contains("Three clips"));
}

#[test]
fn the_panel_speaks_german() {
    let mut d = Driver::demo();
    d.ok("ui.menu.invoke", json!({"id": "app.language.german"}));
    let menu = d.ok("ui.menu.list", json!({}));
    let entry = menu.as_array().unwrap().iter().find(|m| m["id"] == "window.panel.Regie").map(|m| m["label"].clone());
    assert_eq!(entry, Some(json!("KI-Regie")), "Fenster ▸ KI-Regie");
    d.open();
    assert_eq!(d.label("regie.make"), "Film erstellen");
    assert_eq!(d.label("regie.understand"), "Verstehen");
    assert_eq!(d.label("regie.folders.default"), "Bilder, Videos, Musik (die Standardordner)");
    d.click("regie.example.0");
    assert_eq!(d.regie()["request"], "Urlaubsrückblick 16:9 4K, etwa 3 Minuten");
    d.click("regie.understand");
    d.frames(2);
    assert_eq!(d.label("regie.mood"), "Fröhlich");
    assert!(d.label("regie.aspect").starts_with("16:9 · Breitbild"), "{}", d.label("regie.aspect"));
    d.snapshot("german");
    d.ok("ui.menu.invoke", json!({"id": "app.language.english"}));
}

/// The first media item (not a sequence) with a length in a `project.inspect` bin tree.
fn first_media(bin: &Value) -> Option<u64> {
    bin["children"].as_array()?.iter().find_map(|c| match c["item"].as_u64() {
        Some(id) if c["type"] != json!("Sequence") && c["duration"].as_i64().unwrap_or(0) > 0 => Some(id),
        Some(_) => None,
        None => first_media(c),
    })
}

#[test]
fn a_finished_film_plays_and_undoes() {
    let mut d = Driver::demo();
    d.open();
    // a film as the director reports it: a sequence made from a demo clip stands in for it
    let project = d.ok("engine.execute", json!({"command": "project.inspect", "params": {}}));
    let demo_seq = project["activeSequence"].as_u64().expect("the demo's sequence");
    let clip = first_media(&project["root"]).expect("a demo clip");
    let history = |d: &mut Driver| d.ok("engine.execute", json!({"command": "history.list", "params": {}}))["undo"].as_array().cloned().unwrap_or_default();
    let before = history(&mut d).len();
    let seq = d.ok("engine.execute", json!({"command": "file.newSequence", "params": {"name": "Holiday recap", "fromItem": clip}}))["sequence"]
        .as_u64()
        .expect("the new sequence");
    let undo = history(&mut d);
    assert_eq!(undo.len(), before + 1);
    let label = undo.last().and_then(Value::as_str).unwrap_or_default().to_string();
    d.ok("engine.execute", json!({"command": "sequence.open", "params": {"item": demo_seq}}));
    d.ok(
        "ui.set",
        json!({"regie": {"result": {"sequence": seq, "clips": 1, "seconds": 5, "summary": ["One clip"], "undoDepth": before + 1, "undoLabel": label}}}),
    );
    d.frames(2);
    assert_eq!(d.element("regie.play").unwrap()["enabled"], json!(true));
    assert_eq!(d.element("regie.undo").unwrap()["enabled"], json!(true));
    d.click("regie.explain");
    d.snapshot("result");
    d.click("regie.play");
    let ui = d.ok("ui.inspect", json!({}));
    assert_eq!(ui["activeSequence"], json!(seq), "Play opens the film");
    assert_eq!(ui["playback"]["playing"], json!(true), "and plays it");
    assert_eq!(ui["ui"]["regie"]["error"], "");
    d.ok("ui.menu.invoke", json!({"id": "playback.stop"}));
    d.click("regie.undo");
    let r = d.regie();
    assert_eq!(r["result"]["undone"], json!(true), "{r}");
    assert_eq!(history(&mut d).len(), before, "one step back");
    d.frames(2);
    assert_eq!(d.element("regie.undo").unwrap()["enabled"], json!(false), "only once");
    assert_eq!(d.element("regie.play").unwrap()["enabled"], json!(false), "the film is gone");
    assert!(d.label("regie.result").starts_with("The film was taken back."));
}

/// Squeezed into tiny windows, with a plan, a running film and a finished one, the panel draws
/// without a panic (`clamp` with crossed bounds and the like).
#[test]
fn tiny_windows_do_not_break_the_panel() {
    use filmcraft_engine::regie::Plan;
    use filmcraft_ui_egui::panels::regie::{RegieResult, RegieUi};
    let plan = Plan { request: "Reel".into(), understood: vec!["A reel".into()], questions: vec!["Which song?".into()], ..Plan::default() };
    let states = [
        RegieUi { request: "Reel".into(), plan: Some(plan.clone()), folders: vec!["/a/very/long/folder/name/that/does/not/fit".into()], ..Default::default() },
        RegieUi { request: "Reel".into(), plan: Some(plan.clone()), job: Some(1), status: "Finding media".into(), progress: 0.4, ..Default::default() },
        RegieUi {
            request: "Reel".into(),
            plan: Some(plan),
            error: "Something went wrong".into(),
            result: Some(RegieResult {
                sequence: Some(1),
                clips: 1,
                summary: vec!["One clip".into()],
                explain: vec!["File ▸ Import…".into()],
                ..Default::default()
            }),
            open_sections: vec!["folders".into(), "plan".into(), "explain".into()],
            ..Default::default()
        },
    ];
    for (w, h) in [(120.0, 80.0), (300.0, 200.0), (60.0, 40.0)] {
        for state in &states {
            let mut session = Session::default();
            session.execute("file.openDemoProject", json!({})).expect("demo project");
            let mut app = FilmcraftApp::new(session);
            app.ui.regie = state.clone();
            app.show_panel(filmcraft_ui_egui::dock::PanelKind::Regie);
            let mut harness = Harness::builder().with_size(egui::vec2(w, h)).with_max_steps(10_000).build_eframe(move |_cc| app);
            for _ in 0..4 {
                harness.step();
            }
            assert!(harness.state().ui_error.is_none(), "{w}x{h}: {:?}", harness.state().ui_error);
        }
    }
}
