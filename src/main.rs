use adw::prelude::*;
use gapless::library::{self, Track};
use gapless::playlist;
use gapless::player::{Player, PlayerEvent, QueuedTrack, Repeat};
use gapless::settings::Settings;
use gapless::mpris;
use gtk::glib;
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

const APP_ID: &str = "com.procomputation.Gapless";

/// How long after a user seek to ignore pipeline position reports. A flushing
/// seek keeps reporting the *old* position for a moment; without this the
/// slider snaps backwards under the cursor before catching up.
const SEEK_SETTLE: Duration = Duration::from_millis(400);

struct Ui {
    cover: gtk::Image,
    now_playing: gtk::Label,
    now_artist: gtk::Label,
    now_detail: gtk::Label,
    play_button: gtk::Button,
    repeat_button: gtk::Button,
    shuffle_button: gtk::Button,
    volume_icon: gtk::Image,
    seek: gtk::Scale,
    time_label: gtk::Label,
    list: gtk::ListBox,
    tracks: RefCell<Vec<Track>>,
    /// When the user last moved the seek slider. See SEEK_SETTLE.
    last_seek: Cell<Option<Instant>>,
    /// Bumps on every track change so each cached cover gets a fresh filename;
    /// MPRIS clients cache art by URL and won't re-read a path they've seen.
    art_seq: Cell<u64>,
}

fn main() -> glib::ExitCode {
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_window);
    app.run()
}

fn build_window(app: &adw::Application) {
    let player = match Player::new() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("could not start audio engine: {e}");
            return;
        }
    };

    let saved = Settings::load();

    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::Single)
        .css_classes(["boxed-list"])
        .build();
    let scroller = gtk::ScrolledWindow::builder().vexpand(true).child(&list).build();

    // ---- now playing --------------------------------------------------
    let cover = gtk::Image::from_icon_name("audio-x-generic-symbolic");
    cover.set_pixel_size(56);
    cover.add_css_class("card");

    let now_playing = gtk::Label::builder()
        .label("Nothing playing")
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .xalign(0.0)
        .css_classes(["heading"])
        .build();
    let now_artist = gtk::Label::builder()
        .label("Open a folder to begin")
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .xalign(0.0)
        .css_classes(["dim-label"])
        .build();
    let now_detail = gtk::Label::builder()
        .label("")
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .xalign(0.0)
        .css_classes(["dim-label", "caption", "numeric"])
        .build();

    let play_button = gtk::Button::builder()
        .icon_name("media-playback-start-symbolic")
        .css_classes(["circular", "suggested-action"])
        .build();
    let prev_button = gtk::Button::builder()
        .icon_name("media-skip-backward-symbolic")
        .css_classes(["circular", "flat"])
        .build();
    let next_button = gtk::Button::builder()
        .icon_name("media-skip-forward-symbolic")
        .css_classes(["circular", "flat"])
        .build();
    let repeat_button = gtk::Button::builder()
        .icon_name("media-playlist-repeat-symbolic")
        .css_classes(["circular", "flat"])
        .build();
    let shuffle_button = gtk::Button::builder()
        .icon_name("media-playlist-shuffle-symbolic")
        .css_classes(["circular", "flat"])
        .build();

    let seek = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 0.001);
    seek.set_hexpand(true);
    seek.set_draw_value(false);

    let time_label = gtk::Label::builder()
        .label("0:00 / 0:00")
        .css_classes(["numeric", "dim-label", "caption"])
        .build();

    let volume_icon = gtk::Image::from_icon_name("audio-volume-high-symbolic");
    let volume = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 0.01);
    volume.set_draw_value(false);
    volume.set_width_request(110);
    volume.set_tooltip_text(Some("Volume"));

    let ui = Rc::new(Ui {
        cover,
        now_playing,
        now_artist,
        now_detail,
        play_button,
        repeat_button,
        shuffle_button,
        volume_icon,
        seek,
        time_label,
        list,
        tracks: RefCell::new(Vec::new()),
        last_seek: Cell::new(None),
        art_seq: Cell::new(0),
    });

    // ---- layout -------------------------------------------------------
    let text_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    text_box.append(&ui.now_playing);
    text_box.append(&ui.now_artist);
    text_box.append(&ui.now_detail);
    text_box.set_hexpand(true);
    text_box.set_valign(gtk::Align::Center);

    let meta_box = gtk::Box::builder().spacing(12).build();
    meta_box.append(&ui.cover);
    meta_box.append(&text_box);
    meta_box.set_hexpand(true);

    let controls = gtk::Box::builder().spacing(6).valign(gtk::Align::Center).build();
    controls.append(&ui.shuffle_button);
    controls.append(&prev_button);
    controls.append(&ui.play_button);
    controls.append(&next_button);
    controls.append(&ui.repeat_button);

    let top_row = gtk::Box::builder().spacing(12).build();
    top_row.append(&meta_box);
    top_row.append(&controls);

    let seek_row = gtk::Box::builder().spacing(12).build();
    seek_row.append(&ui.seek);
    seek_row.append(&ui.time_label);
    seek_row.append(&ui.volume_icon);
    seek_row.append(&volume);

    let bar = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .margin_top(10)
        .margin_bottom(10)
        .margin_start(12)
        .margin_end(12)
        .build();
    bar.append(&top_row);
    bar.append(&seek_row);

    let open_button = gtk::Button::builder().label("Open Folder…").build();
    let playlist_button = gtk::Button::builder().label("Open Playlist…").build();
    let (prefs_button, xfade_scale, xfade_label, trim_switch) = build_prefs(&player, &saved);

    let header = adw::HeaderBar::new();
    header.pack_start(&open_button);
    header.pack_start(&playlist_button);
    header.pack_end(&prefs_button);
    let _ = (&xfade_scale, &xfade_label, &trim_switch);
    header.set_title_widget(Some(&adw::WindowTitle::new("Gapless", "")));

    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&scroller);
    content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    content.append(&bar);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&content));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Gapless")
        .default_width(820)
        .default_height(640)
        .content(&toolbar)
        .build();

    // ---- restore saved session ----------------------------------------
    player.set_volume(saved.volume);
    volume.set_value(saved.volume);
    player.set_repeat(match saved.repeat.as_str() {
        "all" => Repeat::All,
        "one" => Repeat::One,
        _ => Repeat::Off,
    });
    player.set_shuffle(saved.shuffle);
    player.set_trim_silence(saved.trim_silence);
    player.set_crossfade((saved.crossfade_secs.clamp(0.0, 10.0) * 1e9) as u64);
    player.set_inner_limit((saved.inner_silence_secs.clamp(0.0, 10.0) * 1e9) as u64);
    set_repeat_look(&ui, player.repeat());
    set_shuffle_look(&ui, player.shuffle());

    if let Some(source) = saved.valid_last_source() {
        load_source(&ui, &player, source.to_path_buf());
    }

    wire_up(&window, &ui, &player, &open_button, &playlist_button, &prev_button, &next_button, &volume);
    listen_for_events(&ui, &player);
    persist_on_close(&window, &ui, &player);

    window.present();
}

/// Crossfade and silence-trim live together because they are the two answers to
/// the same complaint. Trimming removes silence that is *in the file*; crossfade
/// overlaps the tracks instead. Crossfade at 0 is exact gapless.
fn build_prefs(
    player: &Arc<Player>,
    saved: &Settings,
) -> (gtk::MenuButton, gtk::Scale, gtk::Label, gtk::Switch) {
    let trim_switch = gtk::Switch::builder()
        .active(saved.trim_silence)
        .valign(gtk::Align::Center)
        .build();
    trim_switch.connect_state_set({
        let player = player.clone();
        move |_, on| {
            player.set_trim_silence(on);
            glib::Propagation::Proceed
        }
    });

    let trim_row = gtk::Box::builder().spacing(12).build();
    let trim_text = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let t1 = gtk::Label::builder().label("Skip silence between tracks").xalign(0.0).build();
    let t2 = gtk::Label::builder()
        .label("Cuts silence recorded into the files themselves")
        .xalign(0.0)
        .css_classes(["dim-label", "caption"])
        .build();
    trim_text.append(&t1);
    trim_text.append(&t2);
    trim_text.set_hexpand(true);
    trim_row.append(&trim_text);
    trim_row.append(&trim_switch);

    let xfade_label = gtk::Label::builder()
        .label(crossfade_text(saved.crossfade_secs))
        .xalign(0.0)
        .css_classes(["dim-label", "caption"])
        .build();

    let xfade_scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 10.0, 0.5);
    xfade_scale.set_value(saved.crossfade_secs.clamp(0.0, 10.0));
    xfade_scale.set_draw_value(false);
    xfade_scale.set_width_request(240);
    xfade_scale.set_hexpand(true);
    for tick in [0.0, 2.0, 4.0, 6.0, 8.0, 10.0] {
        xfade_scale.add_mark(tick, gtk::PositionType::Bottom, None);
    }
    xfade_scale.connect_value_changed({
        let player = player.clone();
        let label = xfade_label.clone();
        move |scale| {
            let secs = scale.value();
            player.set_crossfade((secs * 1e9) as u64);
            label.set_label(&crossfade_text(secs));
        }
    });

    let heading = gtk::Label::builder()
        .label("Crossfade")
        .xalign(0.0)
        .css_classes(["heading"])
        .build();

    // Silence *inside* a track is a different problem from silence between them,
    // and it needs a cap rather than a switch: a four-bar rest is music, five
    // minutes of nothing before a hidden track is not.
    let inner_label = gtk::Label::builder()
        .label(inner_text(saved.inner_silence_secs))
        .xalign(0.0)
        .css_classes(["dim-label", "caption"])
        .build();
    let inner_scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 10.0, 0.5);
    inner_scale.set_value(saved.inner_silence_secs.clamp(0.0, 10.0));
    inner_scale.set_draw_value(false);
    inner_scale.set_width_request(240);
    for tick in [0.0, 2.0, 4.0, 6.0, 8.0, 10.0] {
        inner_scale.add_mark(tick, gtk::PositionType::Bottom, None);
    }
    inner_scale.connect_value_changed({
        let player = player.clone();
        let label = inner_label.clone();
        move |scale| {
            let secs = scale.value();
            player.set_inner_limit((secs * 1e9) as u64);
            label.set_label(&inner_text(secs));
        }
    });
    let inner_heading = gtk::Label::builder()
        .label("Silence inside a track")
        .xalign(0.0)
        .css_classes(["heading"])
        .build();

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    content.append(&trim_row);
    content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    content.append(&heading);
    content.append(&xfade_scale);
    content.append(&xfade_label);
    content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    content.append(&inner_heading);
    content.append(&inner_scale);
    content.append(&inner_label);

    let popover = gtk::Popover::builder().child(&content).build();
    let button = gtk::MenuButton::builder()
        .icon_name("emblem-system-symbolic")
        .tooltip_text("Playback settings")
        .popover(&popover)
        .build();

    (button, xfade_scale, xfade_label, trim_switch)
}

fn inner_text(secs: f64) -> String {
    if secs <= 0.0 {
        "Off — long pauses inside a track play in full".into()
    } else {
        format!("Trim any pause inside a track down to {secs:.1} s")
    }
}

fn crossfade_text(secs: f64) -> String {
    if secs <= 0.0 {
        "Off — tracks play gapless, back to back".into()
    } else {
        format!("{secs:.1} s overlap between tracks")
    }
}

fn wire_up(
    window: &adw::ApplicationWindow,
    ui: &Rc<Ui>,
    player: &Arc<Player>,
    open_button: &gtk::Button,
    playlist_button: &gtk::Button,
    prev_button: &gtk::Button,
    next_button: &gtk::Button,
    volume: &gtk::Scale,
) {
    // The fix for seeking. A GestureClick on a GtkScale never fires `released`:
    // the scale's own drag gesture claims the event sequence, so the click
    // controller is cancelled instead. ::change-value is GtkRange's own signal
    // and reports every move — drag, click-to-position, and keyboard alike.
    ui.seek.connect_change_value({
        let player = player.clone();
        let ui = ui.clone();
        move |_, _, value| {
            ui.last_seek.set(Some(Instant::now()));
            player.seek(value.max(0.0) as u64);
            glib::Propagation::Proceed
        }
    });

    volume.connect_value_changed({
        let player = player.clone();
        let ui = ui.clone();
        move |scale| {
            let v = scale.value();
            player.set_volume(v);
            ui.volume_icon.set_icon_name(Some(match v {
                v if v <= 0.001 => "audio-volume-muted-symbolic",
                v if v < 0.34 => "audio-volume-low-symbolic",
                v if v < 0.67 => "audio-volume-medium-symbolic",
                _ => "audio-volume-high-symbolic",
            }));
        }
    });

    ui.repeat_button.connect_clicked({
        let player = player.clone();
        let ui = ui.clone();
        move |_| {
            let mode = player.repeat().cycle();
            player.set_repeat(mode);
            set_repeat_look(&ui, mode);
        }
    });

    ui.shuffle_button.connect_clicked({
        let player = player.clone();
        let ui = ui.clone();
        move |_| {
            let on = !player.shuffle();
            player.set_shuffle(on);
            set_shuffle_look(&ui, on);
        }
    });

    open_button.connect_clicked({
        let ui = ui.clone();
        let player = player.clone();
        let window = window.clone();
        move |_| {
            let chooser = gtk::FileDialog::builder().title("Choose a music folder").build();
            let ui = ui.clone();
            let player = player.clone();
            chooser.select_folder(Some(&window), gtk::gio::Cancellable::NONE, move |result| {
                let Ok(folder) = result else { return };
                let Some(path) = folder.path() else { return };
                load_source(&ui, &player, path);
            });
        }
    });

    playlist_button.connect_clicked({
        let ui = ui.clone();
        let player = player.clone();
        let window = window.clone();
        move |_| {
            let filter = gtk::FileFilter::new();
            filter.set_name(Some("Playlists (m3u, m3u8, pls)"));
            for pattern in ["*.m3u", "*.m3u8", "*.M3U", "*.M3U8", "*.pls", "*.PLS"] {
                filter.add_pattern(pattern);
            }
            let filters = gtk::gio::ListStore::new::<gtk::FileFilter>();
            filters.append(&filter);

            let chooser = gtk::FileDialog::builder()
                .title("Choose a playlist")
                .filters(&filters)
                .default_filter(&filter)
                .build();

            let ui = ui.clone();
            let player = player.clone();
            chooser.open(Some(&window), gtk::gio::Cancellable::NONE, move |result| {
                let Ok(file) = result else { return };
                let Some(path) = file.path() else { return };
                load_source(&ui, &player, path);
            });
        }
    });

    ui.list.connect_row_activated({
        let player = player.clone();
        move |_, row| {
            if let Err(e) = player.play_index(row.index() as usize) {
                eprintln!("play failed: {e}");
            }
        }
    });

    ui.play_button.connect_clicked({
        let player = player.clone();
        let ui = ui.clone();
        move |_| {
            if !player.is_loaded() && !ui.tracks.borrow().is_empty() {
                let _ = player.play_index(0);
                return;
            }
            if let Err(e) = player.toggle_pause() {
                eprintln!("pause failed: {e}");
            }
        }
    });

    prev_button.connect_clicked({
        let player = player.clone();
        move |_| {
            let _ = player.previous();
        }
    });

    next_button.connect_clicked({
        let player = player.clone();
        move |_| {
            let _ = player.next();
        }
    });
}

/// A folder and a playlist load through the same path, with one crucial
/// difference: `library::scan` sorts by album/disc/track, which is right for a
/// folder and wrong for a playlist. A hand-sequenced set is precisely where
/// gapless matters most, so its order is preserved exactly as written.
fn load_source(ui: &Rc<Ui>, player: &Arc<Player>, path: PathBuf) {
    let (tracks, subtitle) = if path.is_dir() {
        let tracks = library::scan(&path);
        let n = tracks.len();
        (tracks, format!("{n} tracks"))
    } else {
        match playlist::parse(&path) {
            Ok(pl) => {
                let tracks = library::tracks_from_paths(&pl.tracks);
                let mut note = format!("{} · {} tracks", pl.name, tracks.len());
                // Say so rather than silently playing a shorter playlist.
                if !pl.missing.is_empty() {
                    note.push_str(&format!(" · {} missing", pl.missing.len()));
                    for m in &pl.missing {
                        eprintln!("playlist: missing {}", m.display());
                    }
                }
                if pl.remote > 0 {
                    note.push_str(&format!(" · {} remote skipped", pl.remote));
                }
                (tracks, note)
            }
            Err(e) => {
                eprintln!("playlist: {e}");
                ui.now_artist.set_label(&format!("Could not read playlist: {e}"));
                return;
            }
        }
    };

    while let Some(row) = ui.list.first_child() {
        ui.list.remove(&row);
    }

    for track in &tracks {
        let title = gtk::Label::builder()
            .label(&track.title)
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        let sub = gtk::Label::builder()
            .label(format!("{} — {}", track.artist, track.album))
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["dim-label", "caption"])
            .build();

        let text = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .hexpand(true)
            .build();
        text.append(&title);
        text.append(&sub);

        let dur = gtk::Label::builder()
            .label(clock(track.duration_nanos))
            .css_classes(["dim-label", "caption", "numeric"])
            .build();

        let row_box = gtk::Box::builder()
            .spacing(12)
            .margin_top(8)
            .margin_bottom(8)
            .margin_start(12)
            .margin_end(12)
            .build();
        row_box.append(&text);
        row_box.append(&dur);

        ui.list.append(&gtk::ListBoxRow::builder().child(&row_box).build());
    }

    player.set_tracks(
        tracks
            .iter()
            .map(|t| QueuedTrack {
                path: t.path.clone(),
                duration_nanos: t.duration_nanos,
            })
            .collect(),
    );
    *ui.tracks.borrow_mut() = tracks;
    ui.now_artist.set_label(&format!("{subtitle} — press play"));

    // Remember it even if the user never plays anything.
    let mut s = current_settings(ui, player);
    s.last_source = Some(path);
    s.save();
}

fn listen_for_events(ui: &Rc<Ui>, player: &Arc<Player>) {
    let events = player.events.clone();
    let ui = ui.clone();
    let player = player.clone();

    glib::spawn_future_local(async move {
        // MPRIS has to be built inside an async context; if D-Bus is missing we
        // carry on without media keys rather than refusing to start.
        let mpris = mpris::start(player.clone()).await;
        if let Some(m) = &mpris {
            mpris::publish_modes(m, player.repeat(), player.shuffle());
        }

        while let Ok(event) = events.recv().await {
            match event {
                PlayerEvent::TrackStarted(i) => {
                    let track = ui.tracks.borrow().get(i).cloned();
                    let Some(track) = track else { continue };

                    ui.now_playing.set_label(&track.title);
                    ui.now_artist.set_label(&format!("{} — {}", track.artist, track.album));
                    ui.now_detail.set_label(&detail_line(&track));

                    let art = show_cover(&ui, &track);

                    if let Some(row) = ui.list.row_at_index(i as i32) {
                        ui.list.select_row(Some(&row));
                    }
                    if let Some(m) = &mpris {
                        mpris::publish_track(m, i, &track, art.as_ref());
                    }
                }
                PlayerEvent::Position { pos, dur } => {
                    let settled = ui
                        .last_seek
                        .get()
                        .map(|t| t.elapsed() > SEEK_SETTLE)
                        .unwrap_or(true);
                    if settled && dur > 0 {
                        ui.seek.set_range(0.0, dur as f64);
                        ui.seek.set_value(pos as f64);
                        ui.time_label.set_label(&format!("{} / {}", clock(pos), clock(dur)));
                    }
                }
                PlayerEvent::PlayingChanged(playing) => {
                    set_play_icon(&ui, playing);
                    if let Some(m) = &mpris {
                        mpris::publish_status(m, playing);
                    }
                }
                PlayerEvent::QueueFinished => {
                    set_play_icon(&ui, false);
                    ui.now_playing.set_label("Nothing playing");
                    ui.now_artist.set_label("End of queue");
                    ui.now_detail.set_label("");
                    ui.cover.set_icon_name(Some("audio-x-generic-symbolic"));
                }
                PlayerEvent::Error(e) => eprintln!("gstreamer: {e}"),
            }
        }
    });
}

/// Loads embedded art into the header image and drops a copy in the cache dir
/// so MPRIS clients (which want a URL, not bytes) have something to point at.
fn show_cover(ui: &Rc<Ui>, track: &Track) -> Option<PathBuf> {
    let Some(bytes) = library::cover_art(&track.path) else {
        ui.cover.set_icon_name(Some("audio-x-generic-symbolic"));
        return None;
    };

    let glib_bytes = glib::Bytes::from(&bytes);
    match gtk::gdk::Texture::from_bytes(&glib_bytes) {
        Ok(texture) => ui.cover.set_paintable(Some(&texture)),
        Err(_) => {
            ui.cover.set_icon_name(Some("audio-x-generic-symbolic"));
            return None;
        }
    }

    let seq = ui.art_seq.get().wrapping_add(1);
    ui.art_seq.set(seq);

    let dir = glib::user_cache_dir().join("gapless");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(format!("art-{seq}"));
    std::fs::write(&path, &bytes).ok()?;
    Some(path)
}

fn detail_line(track: &Track) -> String {
    let mut parts = Vec::new();
    if let Some(year) = track.year {
        parts.push(year.to_string());
    }
    if let Some(genre) = &track.genre {
        parts.push(genre.clone());
    }
    if !track.format.is_empty() {
        parts.push(track.format.clone());
    }
    parts.join("  ·  ")
}

fn current_settings(ui: &Rc<Ui>, player: &Arc<Player>) -> Settings {
    let mut s = Settings::load();
    s.volume = player.volume();
    s.repeat = match player.repeat() {
        Repeat::Off => "off",
        Repeat::All => "all",
        Repeat::One => "one",
    }
    .into();
    s.shuffle = player.shuffle();
    s.trim_silence = player.trim_silence();
    s.crossfade_secs = player.crossfade() as f64 / 1e9;
    s.inner_silence_secs = player.inner_limit() as f64 / 1e9;
    let _ = ui;
    s
}

fn save_settings(ui: &Rc<Ui>, player: &Arc<Player>) {
    current_settings(ui, player).save();
}

fn persist_on_close(window: &adw::ApplicationWindow, ui: &Rc<Ui>, player: &Arc<Player>) {
    window.connect_close_request({
        let ui = ui.clone();
        let player = player.clone();
        move |_| {
            save_settings(&ui, &player);
            glib::Propagation::Proceed
        }
    });
}

fn set_repeat_look(ui: &Rc<Ui>, mode: Repeat) {
    let (icon, tip) = match mode {
        Repeat::Off => ("media-playlist-repeat-symbolic", "Repeat: off"),
        Repeat::All => ("media-playlist-repeat-symbolic", "Repeat: all tracks"),
        Repeat::One => ("media-playlist-repeat-song-symbolic", "Repeat: this track"),
    };
    ui.repeat_button.set_icon_name(icon);
    ui.repeat_button.set_tooltip_text(Some(tip));
    set_active_look(&ui.repeat_button, mode != Repeat::Off);
}

fn set_shuffle_look(ui: &Rc<Ui>, on: bool) {
    ui.shuffle_button
        .set_tooltip_text(Some(if on { "Shuffle: on" } else { "Shuffle: off" }));
    set_active_look(&ui.shuffle_button, on);
}

/// Off is a flat/quiet button; active modes are highlighted, so the state is
/// legible at a glance rather than only via an icon swap.
fn set_active_look(button: &gtk::Button, active: bool) {
    if active {
        button.remove_css_class("flat");
        button.add_css_class("suggested-action");
    } else {
        button.remove_css_class("suggested-action");
        button.add_css_class("flat");
    }
}

fn set_play_icon(ui: &Rc<Ui>, playing: bool) {
    ui.play_button.set_icon_name(if playing {
        "media-playback-pause-symbolic"
    } else {
        "media-playback-start-symbolic"
    });
}

fn clock(nanos: u64) -> String {
    let secs = nanos / 1_000_000_000;
    format!("{}:{:02}", secs / 60, secs % 60)
}
