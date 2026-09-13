use adw::prelude::*;
use gapless::api::{self, ApiRequest, ApiResponse};
use gapless::autostart;
use gapless::library::{self, Track};
use gapless::playlist;
use gapless::player::{Player, PlayerEvent, QueuedTrack, Repeat, Shuffle};
use gapless::ratings::{self, Ratings};
use gapless::settings::Settings;
use serde_json::{json, Value};
use gapless::mpris;
use gtk::glib;
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

const APP_ID: &str = "com.procomputation.Gapless";

/// Taken from Cargo.toml at compile time, so the number can only be wrong by
/// being wrong in one place. Nothing else in the binary carried its version
/// before this: `strings` on a build found no "0.1.x" anywhere, so there was no
/// way to ask a running or installed copy what it was.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Shown in the About dialog. Says what the program is *for*, because "a music
/// player" does not explain why this one exists.
const ABOUT_TEXT: &str = "\
A music player that plays albums without holes in them.

The gap between tracks has two causes, and fixing only the famous one leaves you \
still hearing a gap: the pipeline stops and restarts between files, and a great \
many rips have silence recorded into them. Gapless fixes both, and can crossfade \
instead if you would rather.

The gapless claim is measured rather than asserted — see docs/VERIFICATION.md.";

/// How long after a user seek to ignore pipeline position reports. A flushing
/// seek keeps reporting the *old* position for a moment; without this the
/// slider snaps backwards under the cursor before catching up.
const SEEK_SETTLE: Duration = Duration::from_millis(800);

/// A seek rebuilds the mixer timeline, which is far too expensive to do on every
/// `change-value` the slider emits — and it emits a burst of them, especially
/// when the trough is clicked rather than the handle dragged. Coalesce: remember
/// the newest target, and only actually seek once the user has settled.
const SEEK_DEBOUNCE: Duration = Duration::from_millis(150);

/// Settings are written whenever one changes, not only on close: the app can be
/// killed, or quit from the terminal it was launched in, and a setting you have
/// to set twice is a setting that isn't remembered. Debounced because dragging
/// the volume slider emits a value per frame.
const SAVE_DEBOUNCE: Duration = Duration::from_millis(600);

/// Favorites shuffle needs its own colour on the shuffle button. It cannot use
/// the star glyph — the rating strip two inches to the left is made of stars,
/// and a star on the transport row would read as "favorite this track" — and it
/// cannot reuse `suggested-action`, which already means plain shuffle. The
/// libadwaita named colours adapt to light and dark themes, so this stays a
/// theme colour rather than a hard-coded gold.
const FAVORITES_CSS: &str = "
button.favorites-shuffle {
    background-color: @warning_bg_color;
    color: @warning_fg_color;
}
";

/// How often the playing position is checkpointed to disk. Frequent enough that
/// a crash costs you a few seconds, rare enough that it isn't a write per tick.
const POSITION_SAVE_EVERY: Duration = Duration::from_secs(5);

struct Ui {
    cover: gtk::Image,
    now_playing: gtk::Label,
    now_artist: gtk::Label,
    now_detail: gtk::Label,
    play_button: gtk::Button,
    repeat_button: gtk::Button,
    shuffle_button: gtk::Button,
    volume_icon: gtk::Image,
    /// Held so the API can move the slider rather than setting the player
    /// behind the UI's back: the slider's own handler is what tells the player,
    /// repaints the icon and schedules the save, so driving the widget keeps an
    /// API call and a drag on exactly the same path.
    volume_scale: gtk::Scale,
    /// Same reasoning, for the three playback settings. `Option` because these
    /// are built by `build_prefs`, which needs the `Ui` that holds them.
    trim_switch: RefCell<Option<gtk::Switch>>,
    xfade_scale: RefCell<Option<gtk::Scale>>,
    inner_scale: RefCell<Option<gtk::Scale>>,
    /// The running control API, if it is switched on. Dropping it stops it.
    api: RefCell<Option<api::Server>>,
    api_tx: async_channel::Sender<ApiRequest>,
    api_key: RefCell<String>,
    seek: gtk::Scale,
    time_label: gtk::Label,
    list: gtk::ListBox,
    /// The five star buttons under the track title. They rate `focus`.
    stars: Vec<gtk::Button>,
    /// One star label per list row, index-parallel to `tracks`. Kept so a rating
    /// change repaints the row without rebuilding the list.
    row_stars: RefCell<Vec<gtk::Label>>,
    ratings: RefCell<Ratings>,
    /// The track the now-playing panel is describing — playing, paused or merely
    /// cued from the last session. This, not the list selection, is what the
    /// star strip and the number keys rate: the strip sits inside the panel, so
    /// rating anything else would be rating a track the panel is not showing.
    focus: Cell<Option<usize>>,
    /// The row a right-click opened the rating menu on. Separate from `focus`
    /// because the whole point of the menu is rating a track that is *not*
    /// the one playing.
    menu_target: Cell<Option<usize>>,
    tracks: RefCell<Vec<Track>>,
    /// When the user last moved the seek slider. See SEEK_SETTLE.
    last_seek: Cell<Option<Instant>>,
    /// Newest requested seek position, and the timer that will apply it.
    seek_target: Cell<Option<u64>>,
    seek_timer: RefCell<Option<glib::SourceId>>,
    /// Bumps on every track change so each cached cover gets a fresh filename;
    /// MPRIS clients cache art by URL and won't re-read a path they've seen.
    art_seq: Cell<u64>,
    save_timer: RefCell<Option<glib::SourceId>>,
    last_pos_save: Cell<Option<Instant>>,
}

fn main() -> glib::ExitCode {
    // Answered before GTK or the audio engine start, so it works headless, over
    // ssh, and against a copy that is already running — none of which can open
    // the About dialog. Handled here rather than left to GApplication, which
    // would need a registered option and a running instance to reply.
    if std::env::args().skip(1).any(|a| a == "--version" || a == "-V") {
        println!("gapless {VERSION}");
        return glib::ExitCode::SUCCESS;
    }

    // Answered here for the same reason as `--version`: a script that wants to
    // drive the API needs the key, and asking a running GTK application for it
    // over the API you have no key for is not a workable plan.
    if std::env::args().skip(1).any(|a| a == "--api-key") {
        match api::load_or_create_key() {
            Some(key) => {
                println!("{key}");
                return glib::ExitCode::SUCCESS;
            }
            None => {
                eprintln!("could not read or create the API key file");
                return glib::ExitCode::FAILURE;
            }
        }
    }

    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_window);
    app.run()
}

/// The About dialog — the only place the *running* app states its version.
/// `--version` covers the installed binary; this covers the copy in front of you.
fn show_about(parent: &impl IsA<gtk::Widget>) {
    // MitX11 is GTK's name for the MIT/X11 licence — the same one in LICENSE and
    // in Cargo.toml's `license` field. All three have to agree; this is the copy
    // a user actually sees.
    let about = adw::AboutDialog::builder()
        .application_name("Gapless")
        .application_icon(APP_ID)
        .version(VERSION)
        .developer_name("PortableDiag")
        .comments(ABOUT_TEXT)
        .website("https://github.com/PortableDiag/gapless")
        .issue_url("https://github.com/PortableDiag/gapless/issues")
        .copyright("© 2026 PortableDiag")
        .license_type(gtk::License::MitX11)
        .build();
    about.present(Some(parent));
}

fn build_window(app: &adw::Application) {
    // The desktop may have `gtk-primary-button-warps-slider` off (KDE ships it
    // that way), which makes clicking a slider's trough page-step towards the
    // pointer instead of jumping to it. For a seek bar that is simply wrong: you
    // click where you want to be. Force it on for this app only.
    if let Some(settings) = gtk::Settings::default() {
        settings.set_gtk_primary_button_warps_slider(true);
    }
    gtk::Window::set_default_icon_name(APP_ID);

    // Whatever previous runs orphaned. Nothing is referencing them: this process
    // has not published an art URL yet, and `art_seq` is about to restart at zero
    // and overwrite the low numbers anyway.
    prune_art_cache(&glib::user_cache_dir().join("gapless"), 0);

    let css = gtk::CssProvider::new();
    css.load_from_string(FAVORITES_CSS);
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &css,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }

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

    // Five flat star buttons. Clicking the star you are already on clears the
    // rating — the same gesture that set it takes it away, so there is no need
    // for a separate "unrate" affordance in the strip.
    let stars: Vec<gtk::Button> = (1..=ratings::MAX)
        .map(|n| {
            gtk::Button::builder()
                .icon_name("non-starred-symbolic")
                .css_classes(["flat", "circular"])
                .tooltip_text(match n {
                    1 => "Rate 1 star".to_string(),
                    n => format!("Rate {n} stars"),
                })
                .build()
        })
        .collect();

    // Requests arrive on the listener's threads and are executed here, on the
    // GTK main thread, in the same place a button click would be.
    let (api_tx, api_rx) = async_channel::unbounded::<ApiRequest>();

    let ui = Rc::new(Ui {
        cover,
        now_playing,
        now_artist,
        now_detail,
        play_button,
        repeat_button,
        shuffle_button,
        volume_icon,
        volume_scale: volume.clone(),
        trim_switch: RefCell::new(None),
        xfade_scale: RefCell::new(None),
        inner_scale: RefCell::new(None),
        api: RefCell::new(None),
        api_tx,
        api_key: RefCell::new(String::new()),
        seek,
        time_label,
        list,
        stars,
        row_stars: RefCell::new(Vec::new()),
        ratings: RefCell::new(Ratings::load()),
        focus: Cell::new(None),
        menu_target: Cell::new(None),
        tracks: RefCell::new(Vec::new()),
        last_seek: Cell::new(None),
        seek_target: Cell::new(None),
        seek_timer: RefCell::new(None),
        art_seq: Cell::new(0),
        save_timer: RefCell::new(None),
        last_pos_save: Cell::new(None),
    });

    // ---- layout -------------------------------------------------------
    // Deliberately in the metadata column, not the transport row: the stars rate
    // the track named directly above them, and putting them beside the shuffle
    // and repeat buttons would make them look like another playback mode.
    let star_row = gtk::Box::builder().halign(gtk::Align::Start).build();
    for button in &ui.stars {
        star_row.append(button);
    }

    let text_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    text_box.append(&ui.now_playing);
    text_box.append(&ui.now_artist);
    text_box.append(&ui.now_detail);
    text_box.append(&star_row);
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
    let prefs_button = build_prefs(&ui, &player, &saved);

    let header = adw::HeaderBar::new();
    header.pack_start(&open_button);
    header.pack_start(&playlist_button);
    header.pack_end(&prefs_button);
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
    player.set_shuffle(Shuffle::from_str(saved.shuffle_mode_str()));
    player.set_trim_silence(saved.trim_silence);
    player.set_crossfade((saved.crossfade_secs.clamp(0.0, 10.0) * 1e9) as u64);
    player.set_inner_limit((saved.inner_silence_secs.clamp(0.0, 10.0) * 1e9) as u64);
    set_repeat_look(&ui, player.repeat());
    set_shuffle_look(&ui, player.shuffle());

    if let Some(source) = saved.valid_last_source() {
        load_source(&ui, &player, source.to_path_buf());
        restore_last_track(&ui, &player, &saved);
    }

    // The key exists whether or not the API is switched on, so the settings
    // popover can show it before you turn it on, and `--api-key` can print it.
    if let Some(key) = api::load_or_create_key() {
        *ui.api_key.borrow_mut() = key;
    }
    if saved.api_enabled {
        match api::start(saved.api_port, ui.api_key.borrow().clone(), ui.api_tx.clone()) {
            Ok(server) => *ui.api.borrow_mut() = Some(server),
            Err(e) => eprintln!("control API could not listen on 127.0.0.1:{}: {e}", saved.api_port),
        }
    }
    serve_api(&ui, &player, api_rx);

    install_rating_actions(app, &window, &ui, &player);
    wire_up(&window, &ui, &player, &open_button, &playlist_button, &prev_button, &next_button, &volume);
    listen_for_events(&ui, &player);
    persist_on_close(&window, &ui, &player);
    persist_on_signal(app, &ui, &player);

    window.present();
}

/// Crossfade and silence-trim live together because they are the two answers to
/// the same complaint. Trimming removes silence that is *in the file*; crossfade
/// overlaps the tracks instead. Crossfade at 0 is exact gapless.
fn build_prefs(ui: &Rc<Ui>, player: &Arc<Player>, saved: &Settings) -> gtk::MenuButton {
    let trim_switch = gtk::Switch::builder()
        .active(saved.trim_silence)
        .valign(gtk::Align::Center)
        .build();
    trim_switch.connect_state_set({
        let player = player.clone();
        let ui = ui.clone();
        move |_, on| {
            player.set_trim_silence(on);
            schedule_save(&ui, &player);
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
        let ui = ui.clone();
        move |scale| {
            let secs = scale.value();
            player.set_crossfade((secs * 1e9) as u64);
            label.set_label(&crossfade_text(secs));
            schedule_save(&ui, &player);
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
        let ui = ui.clone();
        move |scale| {
            let secs = scale.value();
            player.set_inner_limit((secs * 1e9) as u64);
            label.set_label(&inner_text(secs));
            schedule_save(&ui, &player);
        }
    });
    let inner_heading = gtk::Label::builder()
        .label("Silence inside a track")
        .xalign(0.0)
        .css_classes(["heading"])
        .build();

    // Whether we launch at login is the *file's* business, not state.json's —
    // see src/autostart.rs. So the switch reads its initial state from disk.
    let login_switch = gtk::Switch::builder()
        .active(autostart::is_enabled())
        .valign(gtk::Align::Center)
        .build();
    let login_note = gtk::Label::builder()
        .label(login_hint(autostart::is_enabled()))
        .xalign(0.0)
        .wrap(true)
        .css_classes(["dim-label", "caption"])
        .build();
    login_switch.connect_state_set({
        let note = login_note.clone();
        move |sw, on| {
            match autostart::set(on) {
                Ok(()) => note.set_label(login_hint(on)),
                Err(e) => {
                    // Don't leave the switch showing a state we failed to reach.
                    eprintln!("autostart: {e}");
                    note.set_label(&format!("Could not change this: {e}"));
                    sw.set_state(!on);
                    return glib::Propagation::Stop;
                }
            }
            glib::Propagation::Proceed
        }
    });

    let login_row = gtk::Box::builder().spacing(12).build();
    let login_text = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let l1 = gtk::Label::builder().label("Start Gapless when I log in").xalign(0.0).build();
    login_text.append(&l1);
    login_text.append(&login_note);
    login_text.set_hexpand(true);
    login_row.append(&login_text);
    login_row.append(&login_switch);

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
    content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    content.append(&login_row);

    content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    content.append(&build_api_prefs(ui, saved));

    let about_button = gtk::Button::builder()
        .label("About Gapless")
        .css_classes(["flat"])
        .build();
    content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    content.append(&about_button);

    let popover = gtk::Popover::builder().child(&content).build();
    about_button.connect_clicked({
        let popover = popover.clone();
        move |btn| {
            // Dismiss the popover first. Presenting a dialog from a widget that
            // is about to be unmapped leaves it parented to nothing, and it
            // opens detached from the window.
            popover.popdown();
            show_about(btn);
        }
    });
    let button = gtk::MenuButton::builder()
        .icon_name("emblem-system-symbolic")
        .tooltip_text("Playback settings")
        .popover(&popover)
        .build();

    *ui.trim_switch.borrow_mut() = Some(trim_switch);
    *ui.xfade_scale.borrow_mut() = Some(xfade_scale);
    *ui.inner_scale.borrow_mut() = Some(inner_scale);

    button
}

/// The control-API section of the settings popover: the switch, the port, and
/// the key — which has to be *visible and copyable*, because a key the user
/// cannot read is a key they cannot give to the thing that needs it.
fn build_api_prefs(ui: &Rc<Ui>, saved: &Settings) -> gtk::Box {
    let heading = gtk::Label::builder()
        .label("Remote control API")
        .xalign(0.0)
        .css_classes(["heading"])
        .build();

    let note = gtk::Label::builder()
        .label(&api_hint(saved.api_enabled, saved.api_port))
        .xalign(0.0)
        .wrap(true)
        .css_classes(["dim-label", "caption"])
        .build();

    let switch = gtk::Switch::builder()
        .active(saved.api_enabled)
        .valign(gtk::Align::Center)
        .build();

    let port = gtk::SpinButton::with_range(1024.0, 65535.0, 1.0);
    port.set_value(saved.api_port as f64);
    port.set_valign(gtk::Align::Center);
    port.set_tooltip_text(Some("Port on 127.0.0.1"));

    // The key is shown in a read-only entry rather than a label so it can be
    // selected and copied with the keyboard as well as the button.
    let key_entry = gtk::Entry::builder()
        .editable(false)
        .hexpand(true)
        .css_classes(["monospace"])
        .build();
    key_entry.set_text(&ui.api_key.borrow());
    let copy = gtk::Button::builder()
        .icon_name("edit-copy-symbolic")
        .tooltip_text("Copy the key")
        .css_classes(["flat"])
        .valign(gtk::Align::Center)
        .build();
    let regen = gtk::Button::builder()
        .icon_name("view-refresh-symbolic")
        .tooltip_text("Issue a new key — anything using the old one stops working")
        .css_classes(["flat"])
        .valign(gtk::Align::Center)
        .build();

    copy.connect_clicked({
        let key_entry = key_entry.clone();
        move |btn| {
            if let Some(display) = gtk::gdk::Display::default() {
                display.clipboard().set_text(&key_entry.text());
            }
            btn.set_tooltip_text(Some("Copied"));
        }
    });

    regen.connect_clicked({
        let ui = ui.clone();
        let key_entry = key_entry.clone();
        let note = note.clone();
        move |_| {
            let Some(key) = api::regenerate_key() else {
                note.set_label("Could not write the key file");
                return;
            };
            *ui.api_key.borrow_mut() = key.clone();
            key_entry.set_text(&key);
            // The running server captured the old key, so it has to come back up
            // on the new one or the key on screen would be a lie.
            if ui.api.borrow().is_some() {
                let port = ui.api.borrow().as_ref().map(|s| s.port()).unwrap_or(api::DEFAULT_PORT);
                restart_api(&ui, port, &note);
            }
        }
    });

    switch.connect_state_set({
        let ui = ui.clone();
        let note = note.clone();
        let port = port.clone();
        move |_, on| {
            if on {
                restart_api(&ui, port.value() as u16, &note);
            } else {
                // Dropping the Server is what stops the listener.
                *ui.api.borrow_mut() = None;
                note.set_label(&api_hint(false, port.value() as u16));
            }
            save_api_settings(&ui, on, port.value() as u16);
            glib::Propagation::Proceed
        }
    });

    port.connect_value_changed({
        let ui = ui.clone();
        let note = note.clone();
        let switch = switch.clone();
        move |port| {
            let p = port.value() as u16;
            if switch.is_active() {
                restart_api(&ui, p, &note);
            } else {
                note.set_label(&api_hint(false, p));
            }
            save_api_settings(&ui, switch.is_active(), p);
        }
    });

    let switch_row = gtk::Box::builder().spacing(12).build();
    let switch_text = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let s1 = gtk::Label::builder()
        .label("Let other programs control Gapless")
        .xalign(0.0)
        .build();
    switch_text.append(&s1);
    switch_text.append(&note);
    switch_text.set_hexpand(true);
    switch_row.append(&switch_text);
    switch_row.append(&port);
    switch_row.append(&switch);

    let key_row = gtk::Box::builder().spacing(6).build();
    key_row.append(&key_entry);
    key_row.append(&copy);
    key_row.append(&regen);

    let section = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .build();
    section.append(&heading);
    section.append(&switch_row);
    section.append(&key_row);
    section
}

fn api_hint(on: bool, port: u16) -> String {
    if on {
        format!("Listening on http://127.0.0.1:{port}/api — send the key below as a Bearer token")
    } else {
        "Off — nothing is listening".to_string()
    }
}

/// Start, or restart on a new port or key. Stops the old listener first: two
/// servers cannot hold the same port, and starting before stopping would fail
/// with "address in use" every time the port was changed.
fn restart_api(ui: &Rc<Ui>, port: u16, note: &gtk::Label) {
    *ui.api.borrow_mut() = None;
    let key = ui.api_key.borrow().clone();
    match api::start(port, key, ui.api_tx.clone()) {
        Ok(server) => {
            note.set_label(&api_hint(true, port));
            *ui.api.borrow_mut() = Some(server);
        }
        Err(e) => {
            // Say so. A control API that silently failed to start is a long
            // afternoon for whoever is trying to call it.
            note.set_label(&format!("Could not listen on port {port}: {e}"));
        }
    }
}

/// These two are written straight through rather than via `current_settings`,
/// which reconstructs the whole session state from the player.
fn save_api_settings(ui: &Rc<Ui>, enabled: bool, port: u16) {
    let _ = ui;
    let mut s = Settings::load();
    s.api_enabled = enabled;
    s.api_port = port;
    s.save();
}

fn login_hint(on: bool) -> &'static str {
    if on {
        // It restores the queue and cues the track up, but does not start it —
        // same as any other launch. Say so, so nobody expects music at login.
        "Opens on login with your queue restored, paused"
    } else {
        "Off — launch it yourself"
    }
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
            ui.seek_target.set(Some(value.max(0.0) as u64));

            // Restart the timer on every move. Dragging the handle, or clicking
            // the trough (which page-steps repeatedly), fires this many times a
            // second; without coalescing we would tear down and rebuild the
            // pipeline for each one and the UI would lock solid.
            if let Some(id) = ui.seek_timer.borrow_mut().take() {
                id.remove();
            }
            let id = glib::timeout_add_local_once(SEEK_DEBOUNCE, {
                let player = player.clone();
                let ui = ui.clone();
                move || {
                    ui.seek_timer.replace(None);
                    if let Some(target) = ui.seek_target.take() {
                        player.seek(target);
                    }
                }
            });
            ui.seek_timer.replace(Some(id));

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
            schedule_save(&ui, &player);
        }
    });

    // Both of these only set the mode: the resulting ModesChanged event is what
    // repaints the button, republishes to MPRIS, and saves — the same path an
    // MPRIS-originated change takes.
    ui.repeat_button.connect_clicked({
        let player = player.clone();
        move |_| player.set_repeat(player.repeat().cycle())
    });

    ui.shuffle_button.connect_clicked({
        let player = player.clone();
        move |_| player.set_shuffle(player.shuffle().cycle())
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

    // `Player::play_pause` is the whole of "press play": the cued resume point
    // if there is one, else carry on, else the top of the queue. A media key and
    // the control API call the same thing, which is the point — this logic used
    // to live here, where `mpris.rs` could not reach it.
    ui.play_button.connect_clicked({
        let player = player.clone();
        move |_| {
            if let Err(e) = player.play_pause() {
                eprintln!("play failed: {e}");
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

    for (i, button) in ui.stars.iter().enumerate() {
        let want = i as u8 + 1;
        button.connect_clicked({
            let ui = ui.clone();
            let player = player.clone();
            move |_| {
                let Some(track) = ui.focus.get() else { return };
                let now = ui.tracks.borrow().get(track).map(|t| t.rating).unwrap_or(0);
                // Clicking the star you are already on clears it.
                let stars = if now == want { 0 } else { want };
                apply_rating(&ui, &player, track, stars);
            }
        });
    }

    install_row_menu(ui, player);
}

/// Right-click (or long-press, or the Menu key) on any row rates that row.
///
/// The star strip can only rate the track the now-playing panel is showing, and
/// a click on a row in this player *starts* it — so without this, rating a track
/// means playing it first. One gesture and one shared popover, rather than five
/// star buttons per row: the list is not virtualised, and a library of a few
/// thousand tracks already builds a few thousand row widgets.
fn install_row_menu(ui: &Rc<Ui>, player: &Arc<Player>) {
    let menu = gtk::gio::Menu::new();
    for n in (1..=ratings::MAX).rev() {
        menu.append(Some(&ratings::stars_text(n)), Some(&format!("win.rate-row({n})")));
    }
    menu.append(Some("Clear rating"), Some("win.rate-row(0)"));

    let popover = gtk::PopoverMenu::from_model(Some(&menu));
    popover.set_parent(&ui.list);
    popover.set_has_arrow(false);
    popover.set_halign(gtk::Align::Start);

    let gesture = gtk::GestureClick::builder()
        .button(gtk::gdk::BUTTON_SECONDARY)
        .build();
    gesture.connect_pressed({
        let ui = ui.clone();
        let popover = popover.clone();
        let _player = player.clone();
        move |gesture, _, x, y| {
            let Some(row) = ui.list.row_at_y(y as i32) else { return };
            ui.menu_target.set(Some(row.index() as usize));
            gesture.set_state(gtk::EventSequenceState::Claimed);
            popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
            popover.popup();
        }
    });
    ui.list.add_controller(gesture);
}

/// `win.rate` rates whatever the now-playing panel is showing and carries the
/// number-key accelerators; `win.rate-row` rates the row the context menu was
/// opened on. Two actions rather than one with a mutable target, so a stale
/// target can never make a keypress rate the wrong song.
fn install_rating_actions(
    app: &adw::Application,
    window: &adw::ApplicationWindow,
    ui: &Rc<Ui>,
    player: &Arc<Player>,
) {
    let rate = gtk::gio::SimpleAction::new("rate", Some(glib::VariantTy::INT32));
    rate.connect_activate({
        let ui = ui.clone();
        let player = player.clone();
        move |_, param| {
            let Some(stars) = param.and_then(|p| p.get::<i32>()) else { return };
            let Some(track) = ui.focus.get() else { return };
            apply_rating(&ui, &player, track, stars.clamp(0, ratings::MAX as i32) as u8);
        }
    });
    window.add_action(&rate);

    let rate_row = gtk::gio::SimpleAction::new("rate-row", Some(glib::VariantTy::INT32));
    rate_row.connect_activate({
        let ui = ui.clone();
        let player = player.clone();
        move |_, param| {
            let Some(stars) = param.and_then(|p| p.get::<i32>()) else { return };
            let Some(track) = ui.menu_target.take() else { return };
            apply_rating(&ui, &player, track, stars.clamp(0, ratings::MAX as i32) as u8);
        }
    });
    window.add_action(&rate_row);

    // 1–5 rate, 0 clears. Nothing else in this window takes typed input, so the
    // bare digits are free.
    for n in 0..=ratings::MAX as i32 {
        app.set_accels_for_action(&format!("win.rate({n})"), &[&n.to_string()]);
    }
}

/// The one place a rating changes. Writes the sidecar, updates the queue so the
/// next favorites shuffle sees it, and repaints both places it is displayed.
fn apply_rating(ui: &Rc<Ui>, player: &Arc<Player>, track: usize, stars: u8) {
    let path = {
        let tracks = ui.tracks.borrow();
        let Some(t) = tracks.get(track) else { return };
        t.path.clone()
    };

    {
        let mut r = ui.ratings.borrow_mut();
        r.set(&path, stars);
        // Written immediately rather than debounced like `state.json`: this is
        // the one thing on disk the user typed in by hand, and it changes once
        // per click, not once per frame.
        r.save();
    }

    if let Some(t) = ui.tracks.borrow_mut().get_mut(track) {
        t.rating = stars;
    }
    player.set_rating(track, stars);

    if let Some(label) = ui.row_stars.borrow().get(track) {
        label.set_label(&ratings::stars_text(stars));
    }
    if ui.focus.get() == Some(track) {
        set_stars_look(ui, Some(stars));
    }
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

    // Ratings are keyed by path and live outside the scan, so they have to be
    // re-attached every time a source is loaded.
    let mut tracks = tracks;
    {
        let r = ui.ratings.borrow();
        for track in tracks.iter_mut() {
            track.rating = r.get(&track.path);
        }
    }

    // `row_at_index(0)`, not `first_child()`. A GtkListBox's children are not all
    // rows: the rating popover is parented to it, so `first_child()` eventually
    // returns the popover, `remove` refuses it as a non-child, and the loop spins
    // forever — 5.8 million "Tried to remove non-child" warnings a second, with
    // the request that triggered it hung and the window frozen. `row_at_index`
    // only ever returns real rows.
    while let Some(row) = ui.list.row_at_index(0) {
        ui.list.remove(&row);
    }
    ui.row_stars.borrow_mut().clear();

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

        // A label, not five buttons. The list is not virtualised, so anything
        // per-row is paid for once per track in the library; the stars are
        // *set* from the strip, the number keys or the row's own context menu.
        let stars = gtk::Label::builder()
            .label(ratings::stars_text(track.rating))
            .css_classes(["dim-label", "caption"])
            .build();
        ui.row_stars.borrow_mut().push(stars.clone());

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
        row_box.append(&stars);
        row_box.append(&dur);

        ui.list.append(&gtk::ListBoxRow::builder().child(&row_box).build());
    }

    player.set_tracks(
        tracks
            .iter()
            .map(|t| QueuedTrack {
                path: t.path.clone(),
                duration_nanos: t.duration_nanos,
                rating: t.rating,
            })
            .collect(),
    );
    *ui.tracks.borrow_mut() = tracks;
    ui.focus.set(None);
    set_stars_look(ui, None);
    ui.now_artist.set_label(&format!("{subtitle} — press play"));

    // Remember it even if the user never plays anything.
    let mut s = current_settings(ui, player);
    // A resume point belongs to the source it came from. Opening a different
    // folder abandons it rather than resuming into a coincidental path match.
    if s.last_source.as_deref() != Some(path.as_path()) {
        player.clear_cue();
        s.last_track = None;
        s.last_position_secs = 0.0;
    }
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
                    ui.focus.set(Some(i));
                    set_stars_look(&ui, Some(track.rating));

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
                    if let Some(m) = &mpris {
                        mpris::publish_position(m, pos);
                    }

                    let due = ui
                        .last_pos_save
                        .get()
                        .map(|t| t.elapsed() >= POSITION_SAVE_EVERY)
                        .unwrap_or(true);
                    if due && player.is_playing() {
                        ui.last_pos_save.set(Some(Instant::now()));
                        save_settings(&ui, &player);
                    }
                }
                PlayerEvent::PlayingChanged(playing) => {
                    set_play_icon(&ui, playing);
                    if let Some(m) = &mpris {
                        mpris::publish_status(m, playing);
                    }
                    // Pausing is the strongest signal there is that this is where
                    // the user wants to come back to.
                    save_settings(&ui, &player);
                }
                PlayerEvent::ModesChanged { repeat, shuffle } => {
                    set_repeat_look(&ui, repeat);
                    set_shuffle_look(&ui, shuffle);
                    // Echo back to MPRIS so a lock-screen widget that did not
                    // originate the change still sees it.
                    if let Some(m) = &mpris {
                        mpris::publish_modes(m, repeat, shuffle);
                    }
                    schedule_save(&ui, &player);
                }
                PlayerEvent::QueueFinished => {
                    set_play_icon(&ui, false);
                    ui.focus.set(None);
                    set_stars_look(&ui, None);
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

/// How many cover files to leave behind. More than one because an MPRIS client
/// fetches art asynchronously and may still be reading the previous track's
/// file; deleting it the instant the track changes gives the lock screen a
/// broken image. Four is a couple of track changes' worth of grace.
const ART_CACHE_KEEP: usize = 4;

/// Deletes all but the newest `keep` cover files.
///
/// **This was an unbounded leak**: every track change wrote `art-{seq}` and
/// nothing ever removed one. Worse, `art_seq` restarts at zero on every launch,
/// so a new run overwrites `art-1`, `art-2`… and orphans everything above its
/// own high-water mark permanently. Measured on a real install before the fix:
/// **682 MB across 285 files**, for a cache that never needs more than a
/// handful.
///
/// Ordered by the sequence number parsed out of the name, not by mtime — mtimes
/// can be identical or restored out of order, and the number is what the app
/// actually means by "newer".
fn prune_art_cache(dir: &std::path::Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut art: Vec<(u64, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let path = e.path();
            let seq: u64 = path
                .file_name()?
                .to_str()?
                .strip_prefix("art-")?
                .parse()
                .ok()?;
            Some((seq, path))
        })
        .collect();
    if art.len() <= keep {
        return;
    }
    art.sort_unstable_by_key(|(seq, _)| *seq);
    for (_, path) in art.iter().take(art.len() - keep) {
        let _ = std::fs::remove_file(path);
    }
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
    prune_art_cache(&dir, ART_CACHE_KEEP);
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
    s.set_shuffle_mode(player.shuffle().as_str());
    s.trim_silence = player.trim_silence();
    s.crossfade_secs = player.crossfade() as f64 / 1e9;
    s.inner_silence_secs = player.inner_limit() as f64 / 1e9;

    // Only overwrite the resume point if something is actually loaded. Otherwise
    // the first setting the user touches after launch would wipe the position we
    // are holding for them.
    if let Some(i) = player.current() {
        if let Some(track) = ui.tracks.borrow().get(i) {
            s.last_track = Some(track.path.clone());
            s.last_position_secs = player.position() as f64 / 1e9;
        }
    } else if let Some((i, offset)) = player.cued() {
        if let Some(track) = ui.tracks.borrow().get(i) {
            s.last_track = Some(track.path.clone());
            s.last_position_secs = offset as f64 / 1e9;
        }
    }
    s
}

/// Coalesced write. Every control calls this; a volume drag would otherwise
/// rewrite the file once per frame.
fn schedule_save(ui: &Rc<Ui>, player: &Arc<Player>) {
    if let Some(id) = ui.save_timer.borrow_mut().take() {
        id.remove();
    }
    let id = glib::timeout_add_local_once(SAVE_DEBOUNCE, {
        let ui = ui.clone();
        let player = player.clone();
        move || {
            ui.save_timer.replace(None);
            save_settings(&ui, &player);
        }
    });
    ui.save_timer.replace(Some(id));
}

/// Cue up the track the last session was playing, at the position it reached —
/// but do not start it. Matched by path, so a rescan that renumbers the queue
/// still lands on the right song, and a track that has since been deleted simply
/// doesn't resume.
fn restore_last_track(ui: &Rc<Ui>, player: &Arc<Player>, saved: &Settings) {
    let Some(want) = saved.last_track.as_deref() else { return };
    let found = ui
        .tracks
        .borrow()
        .iter()
        .position(|t| t.path == want)
        .map(|i| (i, ui.tracks.borrow()[i].clone()));
    let Some((i, track)) = found else { return };

    let mut pos = (saved.last_position_secs.max(0.0) * 1e9) as u64;
    // Resuming three seconds from the end of a song is not resuming, it is an
    // immediate track change. Treat the tail as "finished" and start it over.
    if track.duration_nanos > 0 && pos + 3_000_000_000 >= track.duration_nanos {
        pos = 0;
    }

    player.set_cued(Some((i, pos)));
    ui.now_playing.set_label(&track.title);
    ui.now_artist.set_label(&format!("{} — {}", track.artist, track.album));
    ui.now_detail.set_label(&detail_line(&track));
    // A cued track is showing in the panel, so it is rateable before it plays.
    ui.focus.set(Some(i));
    set_stars_look(ui, Some(track.rating));
    show_cover(ui, &track);
    if let Some(row) = ui.list.row_at_index(i as i32) {
        ui.list.select_row(Some(&row));
    }
    if track.duration_nanos > 0 {
        ui.seek.set_range(0.0, track.duration_nanos as f64);
        ui.seek.set_value(pos as f64);
        ui.time_label
            .set_label(&format!("{} / {}", clock(pos), clock(track.duration_nanos)));
    }
}

fn save_settings(ui: &Rc<Ui>, player: &Arc<Player>) {
    current_settings(ui, player).save();
}

/// Logging out, `systemctl --user stop`, or a plain `kill` sends SIGTERM, and
/// Ctrl-C sends SIGINT. Neither closes the window, so `persist_on_close` never
/// runs and the resume point dies with the process — the periodic save is the
/// only thing standing between the user and losing their place. A player whose
/// whole point is to remember where you were should not lose it because the
/// session ended rather than the window did.
fn persist_on_signal(app: &adw::Application, ui: &Rc<Ui>, player: &Arc<Player>) {
    const SIGINT: i32 = 2;
    const SIGTERM: i32 = 15;

    for sig in [SIGINT, SIGTERM] {
        glib::unix_signal_add_local(sig, {
            let app = app.clone();
            let ui = ui.clone();
            let player = player.clone();
            move || {
                save_settings(&ui, &player);
                app.quit();
                glib::ControlFlow::Break
            }
        });
    }
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

/// Three states on one button, and they have to be told apart at a glance. The
/// icon stays the same in all three — there is no "shuffle favorites" icon in the
/// theme, and borrowing a star would collide with the rating strip — so the
/// colour carries it: quiet when off, the standard accent for plain shuffle, the
/// warning colour for favorites.
fn set_shuffle_look(ui: &Rc<Ui>, mode: Shuffle) {
    ui.shuffle_button.set_tooltip_text(Some(match mode {
        Shuffle::Off => "Shuffle: off",
        Shuffle::On => "Shuffle: on",
        Shuffle::Favorites => "Shuffle: favorites first — higher-rated tracks come up sooner",
    }));
    set_active_look(&ui.shuffle_button, mode == Shuffle::On);
    if mode == Shuffle::Favorites {
        ui.shuffle_button.remove_css_class("flat");
        ui.shuffle_button.add_css_class("favorites-shuffle");
    } else {
        ui.shuffle_button.remove_css_class("favorites-shuffle");
    }
}

/// `None` means nothing is cued, so there is nothing to rate: the strip goes
/// insensitive rather than showing five empty stars that silently do nothing.
fn set_stars_look(ui: &Rc<Ui>, stars: Option<u8>) {
    let rated = stars.unwrap_or(0);
    for (i, button) in ui.stars.iter().enumerate() {
        button.set_sensitive(stars.is_some());
        button.set_icon_name(if (i as u8) < rated {
            "starred-symbolic"
        } else {
            "non-starred-symbolic"
        });
    }
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

// ---- the control API's command handler ---------------------------------
//
// Everything below runs on the GTK main thread. `src/api.rs` does the sockets,
// the parsing and the key check; this decides what each route *means*, and it
// deliberately goes through the same widgets and the same `Player` calls that a
// click goes through, so an API call and a click cannot drift apart.

/// Reads every request off the channel and answers it. One task, in order —
/// two callers cannot interleave halfway through a queue change.
fn serve_api(ui: &Rc<Ui>, player: &Arc<Player>, rx: async_channel::Receiver<ApiRequest>) {
    let ui = ui.clone();
    let player = player.clone();
    glib::spawn_future_local(async move {
        while let Ok(request) = rx.recv().await {
            let response = dispatch(&ui, &player, &request);
            // The connection thread is blocked waiting on this. A send error
            // only means it gave up first (timeout, client hung up).
            let _ = request.reply.send(response).await;
        }
    });
}

fn dispatch(ui: &Rc<Ui>, player: &Arc<Player>, req: &ApiRequest) -> ApiResponse {
    let get = req.method == "GET";
    let post = req.method == "POST";

    match (req.path.as_str(), get, post) {
        ("/api", true, _) | ("/", true, _) => ApiResponse::ok(json!({
            "ok": true,
            "name": "gapless",
            "version": VERSION,
            "endpoints": {
                "GET  /api/docs":      "the full reference, as Markdown — ?section= narrows it",
                "GET  /api/status":    "everything about the current state",
                "GET  /api/queue":     "the loaded tracks, in playing order",
                "POST /api/play":      "{index?, position_secs?} — omit both to resume",
                "POST /api/pause":     "",
                "POST /api/playpause": "",
                "POST /api/stop":      "",
                "POST /api/next":      "",
                "POST /api/previous":  "",
                "POST /api/seek":      "{position_secs} or {offset_secs}",
                "POST /api/volume":    "{volume: 0.0-1.0}",
                "POST /api/repeat":    "{mode: off|all|one}",
                "POST /api/shuffle":   "{mode: off|on|favorites}",
                "POST /api/rating":    "{index|path, stars: 0-5}",
                "POST /api/open":      "{path} — a folder or a playlist file",
                "GET  /api/settings":  "playback settings",
                "POST /api/settings":  "{trim_silence?, crossfade_secs?, inner_silence_secs?}",
                "GET  /api/autostart": "whether Gapless starts at login",
                "POST /api/autostart": "{enabled}",
                "POST /api/quit":      "close the player",
            }
        })),

        // Served as Markdown, not JSON: the point of it is that somebody reads
        // it, and a 20 KB document escaped into a JSON string is readable by
        // neither a person nor an agent without a second tool.
        ("/api/docs", true, _) => match req.string("section") {
            None => ApiResponse::text("text/markdown; charset=utf-8", api::REFERENCE.to_string()),
            Some(name) => match api::reference_section(&name) {
                Some(section) => ApiResponse::text("text/markdown; charset=utf-8", section),
                // Say which sections exist rather than serving an empty
                // document, which looks like the endpoint is broken.
                None => ApiResponse::error(
                    404,
                    &format!(
                        "no section matching {name:?} — this document has: {}",
                        api::reference_sections().join(", ")
                    ),
                ),
            },
        },

        ("/api/status", true, _) => ApiResponse::ok(status_json(ui, player)),

        ("/api/queue", true, _) => {
            let tracks = ui.tracks.borrow();
            let list: Vec<Value> = tracks
                .iter()
                .enumerate()
                .map(|(i, t)| track_json(i, t))
                .collect();
            ApiResponse::ok(json!({ "ok": true, "count": list.len(), "tracks": list }))
        }

        ("/api/play", _, true) => {
            let index = req.u64("index").map(|i| i as usize);
            let position = req.f64("position_secs").map(|p| (p.max(0.0) * 1e9) as u64);

            let result = match (index, position) {
                (Some(i), pos) => {
                    if i >= ui.tracks.borrow().len() {
                        return ApiResponse::error(404, "no track at that index");
                    }
                    player.play_index_at(i, pos.unwrap_or(0))
                }
                // No index: resume what is cued, else carry on, else start at 0.
                (None, pos) => {
                    if let Some(pos) = pos {
                        match player.current() {
                            Some(_) => {
                                player.seek(pos);
                                Ok(())
                            }
                            None => ApiResponse::error(409, "nothing is loaded to seek in").into_err(),
                        }
                    } else if ui.tracks.borrow().is_empty() {
                        return ApiResponse::error(409, "nothing loaded — POST /api/open first");
                    } else {
                        player.play()
                    }
                }
            };
            match result {
                Ok(()) => ApiResponse::ok(status_json(ui, player)),
                Err(e) => ApiResponse::error(500, &e.to_string()),
            }
        }

        ("/api/pause", _, true) => match player.set_playing(false) {
            Ok(()) => ApiResponse::ok(status_json(ui, player)),
            Err(e) => ApiResponse::error(500, &e.to_string()),
        },

        ("/api/playpause", _, true) => {
            if ui.tracks.borrow().is_empty() {
                return ApiResponse::error(409, "nothing loaded — POST /api/open first");
            }
            let result = player.play_pause();
            match result {
                Ok(()) => ApiResponse::ok(status_json(ui, player)),
                Err(e) => ApiResponse::error(500, &e.to_string()),
            }
        }

        ("/api/stop", _, true) => match player.stop() {
            Ok(()) => ApiResponse::ok(status_json(ui, player)),
            Err(e) => ApiResponse::error(500, &e.to_string()),
        },

        ("/api/next", _, true) => match player.next() {
            Ok(()) => ApiResponse::ok(status_json(ui, player)),
            Err(e) => ApiResponse::error(500, &e.to_string()),
        },

        ("/api/previous", _, true) => match player.previous() {
            Ok(()) => ApiResponse::ok(status_json(ui, player)),
            Err(e) => ApiResponse::error(500, &e.to_string()),
        },

        ("/api/seek", _, true) => {
            if player.current().is_none() {
                return ApiResponse::error(409, "nothing is playing");
            }
            let target = match (req.f64("position_secs"), req.f64("offset_secs")) {
                (Some(p), _) => (p.max(0.0) * 1e9) as u64,
                (None, Some(o)) => {
                    let now = player.position() as f64;
                    ((now + o * 1e9).max(0.0)) as u64
                }
                (None, None) => {
                    return ApiResponse::error(400, "send position_secs or offset_secs")
                }
            };
            player.seek(target);
            ApiResponse::ok(status_json(ui, player))
        }

        ("/api/volume", _, true) => {
            let Some(v) = req.f64("volume") else {
                return ApiResponse::error(400, "send volume, 0.0 to 1.0");
            };
            // Move the slider, don't set the player: the slider's handler is what
            // tells the player, repaints the icon and schedules the save.
            ui.volume_scale.set_value(v.clamp(0.0, 1.0));
            ApiResponse::ok(status_json(ui, player))
        }

        ("/api/repeat", _, true) => {
            let Some(mode) = req.string("mode") else {
                return ApiResponse::error(400, "send mode: off, all or one");
            };
            let repeat = match mode.as_str() {
                "off" => Repeat::Off,
                "all" => Repeat::All,
                "one" => Repeat::One,
                other => {
                    return ApiResponse::error(400, &format!("unknown repeat mode {other:?} — use off, all or one"))
                }
            };
            player.set_repeat(repeat);
            ApiResponse::ok(status_json(ui, player))
        }

        ("/api/shuffle", _, true) => {
            let Some(mode) = req.string("mode") else {
                return ApiResponse::error(400, "send mode: off, on or favorites");
            };
            // `Shuffle::from_str` maps anything unknown to Off, which is right
            // when reading a config file and wrong here — a typo in a script
            // should be an error, not a silent mode change.
            if !matches!(mode.as_str(), "off" | "on" | "favorites") {
                return ApiResponse::error(400, &format!("unknown shuffle mode {mode:?} — use off, on or favorites"));
            }
            player.set_shuffle(Shuffle::from_str(&mode));
            ApiResponse::ok(status_json(ui, player))
        }

        ("/api/rating", _, true) => {
            let Some(stars) = req.u64("stars") else {
                return ApiResponse::error(400, "send stars, 0 to 5 (0 clears)");
            };
            if stars > ratings::MAX as u64 {
                return ApiResponse::error(400, "stars must be 0 to 5");
            }
            // By index, or by path for a caller that has one and does not want to
            // search the queue for it.
            let index = match (req.u64("index"), req.string("path")) {
                (Some(i), _) => i as usize,
                (None, Some(path)) => {
                    let want = PathBuf::from(path);
                    match ui.tracks.borrow().iter().position(|t| t.path == want) {
                        Some(i) => i,
                        None => return ApiResponse::error(404, "that path is not in the current queue"),
                    }
                }
                // Neither: rate what is playing, which is what the star strip does.
                (None, None) => match focused_track(ui, player) {
                    Some(i) => i,
                    None => return ApiResponse::error(409, "nothing is playing — send index or path"),
                },
            };
            if index >= ui.tracks.borrow().len() {
                return ApiResponse::error(404, "no track at that index");
            }
            apply_rating(ui, player, index, stars as u8);
            let track = ui.tracks.borrow()[index].clone();
            ApiResponse::ok(json!({ "ok": true, "track": track_json(index, &track) }))
        }

        ("/api/open", _, true) => {
            let Some(path) = req.string("path") else {
                return ApiResponse::error(400, "send path: a folder or a playlist file");
            };
            let path = PathBuf::from(path);
            if !path.exists() {
                return ApiResponse::error(404, "no such file or folder");
            }
            load_source(ui, player, path);
            let count = ui.tracks.borrow().len();
            if count == 0 {
                // Not an error — an empty folder is a real answer — but say so
                // rather than reporting a successful load of nothing.
                return ApiResponse::ok(json!({
                    "ok": true,
                    "count": 0,
                    "note": "loaded, but no playable tracks were found there"
                }));
            }
            ApiResponse::ok(json!({ "ok": true, "count": count }))
        }

        ("/api/settings", true, _) => ApiResponse::ok(json!({
            "ok": true,
            "trim_silence": player.trim_silence(),
            "crossfade_secs": player.crossfade() as f64 / 1e9,
            "inner_silence_secs": player.inner_limit() as f64 / 1e9,
        })),

        ("/api/settings", _, true) => {
            let mut changed = Vec::new();
            // Each of these drives the widget, so the popover shows the truth the
            // next time it is opened and the label under the slider updates too.
            if let Some(on) = req.bool("trim_silence") {
                if let Some(sw) = ui.trim_switch.borrow().as_ref() {
                    sw.set_active(on);
                } else {
                    player.set_trim_silence(on);
                }
                changed.push("trim_silence");
            }
            if let Some(secs) = req.f64("crossfade_secs") {
                if !(0.0..=10.0).contains(&secs) {
                    return ApiResponse::error(400, "crossfade_secs must be 0 to 10");
                }
                match ui.xfade_scale.borrow().as_ref() {
                    Some(scale) => scale.set_value(secs),
                    None => player.set_crossfade((secs * 1e9) as u64),
                }
                changed.push("crossfade_secs");
            }
            if let Some(secs) = req.f64("inner_silence_secs") {
                if !(0.0..=10.0).contains(&secs) {
                    return ApiResponse::error(400, "inner_silence_secs must be 0 to 10");
                }
                match ui.inner_scale.borrow().as_ref() {
                    Some(scale) => scale.set_value(secs),
                    None => player.set_inner_limit((secs * 1e9) as u64),
                }
                changed.push("inner_silence_secs");
            }
            if changed.is_empty() {
                return ApiResponse::error(400, "send at least one of trim_silence, crossfade_secs, inner_silence_secs");
            }
            ApiResponse::ok(json!({
                "ok": true,
                "changed": changed,
                "trim_silence": player.trim_silence(),
                "crossfade_secs": player.crossfade() as f64 / 1e9,
                "inner_silence_secs": player.inner_limit() as f64 / 1e9,
            }))
        }

        ("/api/autostart", true, _) => {
            ApiResponse::ok(json!({ "ok": true, "enabled": autostart::is_enabled() }))
        }

        ("/api/autostart", _, true) => {
            let Some(on) = req.bool("enabled") else {
                return ApiResponse::error(400, "send enabled: true or false");
            };
            match autostart::set(on) {
                Ok(()) => ApiResponse::ok(json!({ "ok": true, "enabled": autostart::is_enabled() })),
                Err(e) => ApiResponse::error(500, &e.to_string()),
            }
        }

        ("/api/quit", _, true) => {
            save_settings(ui, player);
            // Queued rather than immediate: the reply has to reach the caller
            // before the process goes away, and we are inside the handler that
            // produces it.
            glib::idle_add_local_once(|| {
                if let Some(app) = gtk::gio::Application::default() {
                    app.quit();
                }
            });
            ApiResponse::ok(json!({ "ok": true, "quitting": true }))
        }

        // A known route with the wrong verb is a different mistake from a route
        // that does not exist, and saying so saves a round of guessing.
        (path, _, _) if known_route(path) => ApiResponse::error(
            405,
            &format!("{} is not allowed on {path} — see GET /api", req.method),
        ),
        _ => ApiResponse::error(404, "no such endpoint — see GET /api"),
    }
}

fn known_route(path: &str) -> bool {
    matches!(
        path,
        "/api/docs"
            | "/api/status"
            | "/api/queue"
            | "/api/play"
            | "/api/pause"
            | "/api/playpause"
            | "/api/stop"
            | "/api/next"
            | "/api/previous"
            | "/api/seek"
            | "/api/volume"
            | "/api/repeat"
            | "/api/shuffle"
            | "/api/rating"
            | "/api/open"
            | "/api/settings"
            | "/api/autostart"
            | "/api/quit"
    )
}

fn track_json(index: usize, t: &Track) -> Value {
    json!({
        "index": index,
        "title": t.title,
        "artist": t.artist,
        "album": t.album,
        "year": t.year,
        "genre": t.genre,
        "disc": t.disc,
        "track_no": t.track_no,
        "duration_secs": t.duration_nanos as f64 / 1e9,
        "format": t.format,
        "rating": t.rating,
        "path": t.path,
    })
}

/// "The current track", for anything that has to answer the question without a
/// track number in hand.
///
/// **`ui.focus` alone is not enough**, and driving the API by hand is what showed
/// it: `focus` is set by the `TrackStarted` event, which arrives on the channel
/// *after* the call that started playback has already returned. So a
/// `POST /api/play {"index":6}` answered `"track": null`, and a
/// `POST /api/rating {"stars":5}` answered 409 "nothing is playing" — while it
/// was playing. `player.current()` is set synchronously by `start_at`, so it
/// leads; `focus` covers the track merely cued from the last session, which the
/// panel shows before anything has started.
fn focused_track(ui: &Rc<Ui>, player: &Arc<Player>) -> Option<usize> {
    player
        .current()
        .or_else(|| ui.focus.get())
        .or_else(|| player.cued().map(|(i, _)| i))
}

fn status_json(ui: &Rc<Ui>, player: &Arc<Player>) -> Value {
    // Playing, paused, or merely cued from the last session.
    let current = focused_track(ui, player);
    let track = current.and_then(|i| ui.tracks.borrow().get(i).cloned());

    // A cued track has a position even though nothing is loaded yet — it is the
    // offset the resume will start at. Reporting 0 there told a caller the track
    // was at the beginning when it was two minutes in, and the number changed
    // under them the instant they pressed play.
    let position = match player.current() {
        Some(_) => player.position(),
        None => player.cued().map(|(_, offset)| offset).unwrap_or(0),
    };

    json!({
        "ok": true,
        "version": VERSION,
        "playing": player.is_playing(),
        "loaded": player.is_loaded(),
        "position_secs": position as f64 / 1e9,
        "volume": player.volume(),
        "repeat": match player.repeat() {
            Repeat::Off => "off",
            Repeat::All => "all",
            Repeat::One => "one",
        },
        "shuffle": player.shuffle().as_str(),
        "trim_silence": player.trim_silence(),
        "crossfade_secs": player.crossfade() as f64 / 1e9,
        "inner_silence_secs": player.inner_limit() as f64 / 1e9,
        "queue_length": ui.tracks.borrow().len(),
        "source": Settings::load().last_source,
        "track": track.map(|t| track_json(current.unwrap_or(0), &t)),
    })
}

/// `?` over `anyhow::Result` and an early `ApiResponse` do not mix in the one
/// place `/api/play` needs both. This keeps that branch readable.
trait IntoErr {
    fn into_err(self) -> anyhow::Result<()>;
}

impl IntoErr for ApiResponse {
    fn into_err(self) -> anyhow::Result<()> {
        Err(anyhow::anyhow!(self
            .body
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("error")
            .to_string()))
    }
}

#[cfg(test)]
mod art_cache_tests {
    use super::*;

    fn write(dir: &std::path::Path, name: &str) {
        std::fs::write(dir.join(name), b"jpeg").unwrap();
    }

    fn names(dir: &std::path::Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    /// The leak this closes was measured at 682 MB across 285 files on a real
    /// install: every track change wrote one and nothing ever removed one.
    #[test]
    fn only_the_newest_covers_survive() {
        let dir = std::env::temp_dir().join(format!("gapless-art-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        for n in 1..=10 {
            write(&dir, &format!("art-{n}"));
        }
        prune_art_cache(&dir, 4);
        assert_eq!(names(&dir), vec!["art-10", "art-7", "art-8", "art-9"]);

        // keep: 0 is the startup sweep — nothing is referencing anything yet.
        prune_art_cache(&dir, 0);
        assert!(names(&dir).is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Ordered by the sequence number, not lexically. `art-9` is NEWER than
    /// `art-10` by string order, and sorting that way would delete the newest
    /// cover and keep nine stale ones.
    #[test]
    fn ordering_is_numeric_not_lexical() {
        let dir = std::env::temp_dir().join(format!("gapless-art-lex-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for n in [2u64, 9, 10, 11] {
            write(&dir, &format!("art-{n}"));
        }
        prune_art_cache(&dir, 2);
        assert_eq!(names(&dir), vec!["art-10", "art-11"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Anything that is not a cover file must be left alone, and fewer files
    /// than `keep` must not error.
    #[test]
    fn it_touches_nothing_else_and_tolerates_an_empty_cache() {
        let dir = std::env::temp_dir().join(format!("gapless-art-other-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        write(&dir, "art-1");
        write(&dir, "not-art");
        write(&dir, "art-notanumber");
        prune_art_cache(&dir, 0);
        assert_eq!(names(&dir), vec!["art-notanumber", "not-art"]);

        // A directory that does not exist at all must not panic.
        prune_art_cache(&dir.join("missing"), 4);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
