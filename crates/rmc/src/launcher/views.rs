//! Launcher pages after the mockup (docs/design): header with logo
//! and navigation, "Verbinden" (new connection, current/recent connections, favorites),
//! "Verlauf", "Log", "Einstellungen", status bar.

use std::time::Duration;

use eframe::egui::{self, Align, Layout, RichText, vec2};

use super::icons::{self, Icon};
use super::theme::{self, Kind};
use super::{Launcher, Running, State, Target, history, logview, reach_color};
use crate::profiles::Favorite;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Page {
    Connect,
    History,
    Log,
    Settings,
}

/// What a click in a list asks for (applied after drawing).
enum Act {
    Connect(usize),
    Edit(usize),
    Dup(usize),
    Forget(usize),
    Delete(usize),
    Disconnect(String),
    Dismiss(String),
    EditSession(String),
    ShowLog(std::path::PathBuf),
    SaveAdhoc(Favorite),
    KeepDrive(String, crate::profiles::Drive),
    ConnectRecent(history::Entry),
}

impl Launcher {
    pub(super) fn draw(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        egui::Panel::top("header")
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(theme::BG)
                    .inner_margin(egui::Margin {
                        left: 20,
                        right: 20,
                        top: 12,
                        bottom: 0,
                    }),
            )
            .show(ui, |ui| self.header(ui));
        egui::Panel::bottom("footer")
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(theme::CARD)
                    .stroke(egui::Stroke::new(1.0, theme::BORDER))
                    .inner_margin(egui::Margin::symmetric(20, 10)),
            )
            .show(ui, |ui| self.footer(ui));
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme::BG)
                    .inner_margin(egui::Margin::same(20)),
            )
            .show(ui, |ui| {
                if self.editor.is_some() {
                    self.editor_page(ui);
                    return;
                }
                match self.page {
                    Page::Connect => self.connect_page(ui),
                    Page::History => self.history_page(ui),
                    Page::Log => self.log_page(ui),
                    Page::Settings => {
                        if self.editor.is_none() {
                            self.open_editor(Target::Defaults, Favorite::default());
                        }
                        self.editor_page(ui);
                    }
                }
            });
        self.delete_dialog(&ctx);
        self.password_dialog(&ctx);
        self.cert_dialog(&ctx);
    }

    // ---- header ----------------------------------------------------------------------------

    fn header(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let tex = self.logo.get_or_insert_with(|| {
                let (w, h, px) =
                    crate::icons::rgba(crate::icons::LOGO).unwrap_or((1, 1, vec![0; 4]));
                ui.ctx().load_texture(
                    "rmc-logo",
                    egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &px),
                    egui::TextureOptions::LINEAR,
                )
            });
            ui.add(egui::Image::new((tex.id(), vec2(74.0, 74.0))));
            ui.add_space(10.0);
            let (r, _) = ui.allocate_exact_size(vec2(1.0, 40.0), egui::Sense::hover());
            ui.painter().rect_filled(r, 0.0, theme::BORDER);
            ui.add_space(14.0);
            let items = [
                (Page::Connect, Icon::Monitor, "Verbinden"),
                (Page::History, Icon::Clock, "Verlauf"),
                (Page::Log, Icon::File, "Log"),
                (Page::Settings, Icon::Gear, "Einstellungen"),
            ];
            for (page, icon, label) in items {
                let sel = self.page == page && (self.editor.is_none() || page == Page::Settings);
                if nav_button(ui, icon, label, sel).clicked() {
                    // leaving the favorite editor discards it; the settings page saves itself
                    self.editor = None;
                    self.page = page;
                    if page == Page::Log {
                        self.log_latest_if_none();
                    }
                }
            }
        });
        ui.add_space(6.0);
        let r = ui.max_rect();
        ui.painter().hline(
            r.left()..=r.right(),
            ui.cursor().top(),
            egui::Stroke::new(1.0, theme::BORDER),
        );
    }

    // ---- connect page ----------------------------------------------------------------------

    fn connect_page(&mut self, ui: &mut egui::Ui) {
        let avail = ui.available_size();
        let right_w = (avail.x * 0.38).clamp(340.0, 520.0);
        let left_w = avail.x - right_w - 18.0;
        let mut acts: Vec<Act> = Vec::new();
        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(vec2(left_w, avail.y), Layout::top_down(Align::Min), |ui| {
                ui.set_max_width(left_w);
                // scrolls only when the window is lower than the content
                egui::ScrollArea::vertical()
                    .id_salt("connect_left")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.set_max_width(left_w - 12.0);
                        self.new_connection_card(ui);
                        ui.add_space(14.0);
                        self.connections_card(ui, &mut acts);
                    });
            });
            ui.add_space(18.0);
            ui.allocate_ui_with_layout(
                vec2(right_w, avail.y),
                Layout::top_down(Align::Min),
                |ui| {
                    ui.set_max_width(right_w);
                    self.favorites_card(ui, &mut acts);
                },
            );
        });
        self.apply_acts(acts);
    }

    fn new_connection_card(&mut self, ui: &mut egui::Ui) {
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            card_title(
                ui,
                Icon::Monitor,
                "Neue Verbindung",
                "Mit einem entfernten Windows-Computer verbinden",
            );
            ui.add_space(10.0);
            let entra = self.new_entra;
            let col = ((ui.available_width() - 2.0 * 86.0 - 24.0) / 2.0).max(140.0);
            let mut go = false;
            let enter = |ui: &egui::Ui, r: &egui::Response| {
                r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))
            };
            ui.horizontal(|ui| {
                label_w(ui, "Ziel / Host", 86.0);
                let r = theme::field(ui, Icon::Server, &mut self.new_host, "rechner.domain", col, true, None);
                go |= enter(ui, &r);
                ui.add_space(12.0);
                label_w(ui, "Domäne", 74.0);
                theme::field(
                    ui,
                    Icon::Building,
                    &mut self.new_domain,
                    if entra { "nicht bei Entra ID" } else { "Domäne (optional)" },
                    col,
                    !entra,
                    None,
                );
            });
            ui.horizontal(|ui| {
                label_w(ui, "Benutzer", 86.0);
                let hint = if entra { "name@firma.de" } else { "Benutzername" };
                let r = theme::field(ui, Icon::User, &mut self.new_user, hint, col, true, None);
                go |= enter(ui, &r);
                ui.add_space(12.0);
                label_w(ui, "Passwort", 74.0);
                let r = theme::field(
                    ui,
                    Icon::Lock,
                    &mut self.new_password,
                    if entra { "Anmeldung im Browserfenster" } else { "Passwort" },
                    col,
                    !entra,
                    Some(&mut self.new_show_pw),
                );
                go |= enter(ui, &r);
            });
            ui.horizontal(|ui| {
                ui.add_space(90.0);
                ui.checkbox(&mut self.new_entra, "Entra-ID verwenden")
                    .on_hover_text("Für Firmenrechner mit Entra-ID-Anmeldung (Passwort und 2FA im Browserfenster). Aus: Anmeldung mit Benutzer und Passwort, z. B. für Rechner in einer Domäne oder mit lokalem Konto.");
            });
            ui.add_space(4.0);
            let row = vec2(ui.available_width(), 36.0);
            ui.allocate_ui_with_layout(row, Layout::right_to_left(Align::Center), |ui| {
                let more = theme::button(ui, Some(Icon::Dots), "", Kind::Icon, 0.0);
                egui::Popup::menu(&more).show(|ui| {
                    if ui.button("Erweiterte Optionen (als Favorit) …").clicked() {
                        let fav = self.new_target_fav();
                        self.open_editor(Target::Favorite(None), fav);
                    }
                    if ui.button("Felder leeren").clicked() {
                        self.new_host.clear();
                        self.new_user.clear();
                        self.new_domain.clear();
                        self.new_password.clear();
                    }
                });
                if theme::button(ui, Some(Icon::Star), "", Kind::Icon, 0.0)
                    .on_hover_text("Als Favorit speichern")
                    .clicked()
                {
                    let fav = self.new_target_fav();
                    self.open_editor(Target::Favorite(None), fav);
                }
                let can = !self.new_host.trim().is_empty();
                if theme::button_enabled(ui, Some(Icon::Plug), "Verbinden", Kind::Primary, 160.0, can)
                    .clicked()
                {
                    go = true;
                }
            });
            if go && !self.new_host.trim().is_empty() {
                let fav = self.new_target_fav();
                let pw = std::mem::take(&mut self.new_password);
                if fav.entra || pw.is_empty() {
                    self.connect(fav, true);
                } else {
                    self.start(fav, true, Some(pw), None);
                }
            }
        });
    }

    /// The "Neue Verbindung" fields as a favorite.
    fn new_target_fav(&self) -> Favorite {
        let host = self.new_host.trim().to_string();
        let mut fav = Favorite::new(&host, &host, self.new_user.trim());
        fav.entra = self.new_entra;
        if !fav.entra {
            fav.domain = self.new_domain.trim().to_string();
        }
        fav
    }

    fn connections_card(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            let h = ui.available_height() - 34.0;
            if h.is_finite() {
                ui.set_min_height(h.max(0.0));
            }
            let running = self.sessions.iter().filter(|s| s.running()).count();
            let first = if running > 1 {
                format!("Aktuelle Verbindungen ({running})")
            } else {
                "Aktuelle Verbindung".to_string()
            };
            if let Some(t) = theme::tabs(
                ui,
                &[
                    (Icon::Monitor, first.as_str()),
                    (Icon::Clock, "Letzte Verbindungen"),
                ],
                self.conn_tab,
            ) {
                self.conn_tab = t;
            }
            ui.add_space(10.0);
            if self.conn_tab == 1 {
                self.recent_list(ui, acts);
                return;
            }
            if let Some(fav) = self.offer_save.clone() {
                egui::Frame::new()
                    .fill(theme::CARD_HI)
                    .corner_radius(8)
                    .inner_margin(egui::Margin::symmetric(12, 8))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.label(format!("„{}“ als Favorit speichern?", fav.host));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if ui.button("Nein").clicked() {
                                    self.offer_save = None;
                                }
                                if ui.button("Speichern …").clicked() {
                                    acts.push(Act::SaveAdhoc(fav.clone()));
                                }
                            });
                        });
                    });
                ui.add_space(8.0);
            }
            if let Some((fav, drive)) = self.offer_drive.clone() {
                egui::Frame::new()
                    .fill(theme::CARD_HI)
                    .corner_radius(8)
                    .inner_margin(egui::Margin::symmetric(12, 8))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.label(format!(
                                "Laufwerk „{}“ ({}) dauerhaft in „{fav}“ verbinden?",
                                drive.name, drive.path
                            ));
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if ui.button("Nein").clicked() {
                                    self.offer_drive = None;
                                }
                                if ui.button("Speichern").clicked() {
                                    acts.push(Act::KeepDrive(fav.clone(), drive.clone()));
                                }
                            });
                        });
                    });
                ui.add_space(8.0);
            }
            if self.sessions.is_empty() {
                ui.add_space(24.0);
                ui.vertical_centered(|ui| {
                    ui.label(theme::weak("Keine Verbindung offen."));
                    ui.label(
                        RichText::new(
                            "Einen Favoriten rechts verbinden oder oben ein Ziel eingeben.",
                        )
                        .small()
                        .color(theme::GREY),
                    );
                });
                return;
            }
            if self.sessions.len() == 1 {
                let s = &self.sessions[0];
                session_big(ui, s, acts);
            } else {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        for s in &self.sessions {
                            session_row(ui, s, acts);
                            ui.add_space(6.0);
                        }
                    });
            }
            if self
                .sessions
                .iter()
                .any(|s| s.running() && s.started.elapsed() < Duration::from_secs(2))
            {
                ui.ctx().request_repaint();
            }
        });
    }

    fn recent_list(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        let recent: Vec<history::Entry> = history::recent(&self.history, 8)
            .into_iter()
            .cloned()
            .collect();
        if recent.is_empty() {
            ui.add_space(16.0);
            ui.vertical_centered(|ui| ui.label(theme::weak("Noch keine Verbindungen.")));
            return;
        }
        egui::ScrollArea::vertical()
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for e in recent {
                    row_frame(false).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            let (r, _) =
                                ui.allocate_exact_size(vec2(22.0, 22.0), egui::Sense::hover());
                            icons::paint(ui.painter(), r, Icon::Clock, theme::WEAK);
                            ui.vertical(|ui| {
                                ui.spacing_mut().item_spacing.y = 2.0;
                                ui.label(RichText::new(&e.name).strong());
                                ui.label(theme::weak(format!("{} · {}", e.host, e.user)).small());
                            });
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if theme::button(ui, Some(Icon::Plug), "", Kind::IconAccent, 0.0)
                                    .on_hover_text("Verbinden")
                                    .clicked()
                                {
                                    acts.push(Act::ConnectRecent(e.clone()));
                                }
                                ui.label(theme::weak(short_time(&e.started)).small());
                            });
                        });
                    });
                    ui.add_space(4.0);
                }
            });
    }

    fn favorites_card(&mut self, ui: &mut egui::Ui, acts: &mut Vec<Act>) {
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.set_min_height(ui.available_height() - 34.0);
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(vec2(26.0, 26.0), egui::Sense::hover());
                icons::paint(ui.painter(), r, Icon::Star, theme::TEXT);
                ui.label(RichText::new("Favoriten").heading());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let more = theme::button(ui, Some(Icon::Dots), "", Kind::Icon, 0.0);
                    egui::Popup::menu(&more).show(|ui| {
                        if ui.button("Erreichbarkeit jetzt prüfen").clicked() {
                            self.restart_reach();
                        }
                        if ui.button("Standard-Einstellungen …").clicked() {
                            self.page = Page::Settings;
                        }
                    });
                    if theme::button(ui, Some(Icon::Plus), "Neuer Favorit", Kind::Secondary, 0.0)
                        .clicked()
                    {
                        self.open_editor(Target::Favorite(None), Favorite::new("", "", ""));
                    }
                });
            });
            ui.add_space(6.0);
            theme::field(
                ui,
                Icon::Search,
                &mut self.fav_search,
                "Favoriten durchsuchen …",
                ui.available_width(),
                true,
                None,
            );
            ui.add_space(8.0);
            let q = self.fav_search.trim().to_lowercase();
            let matches = |f: &Favorite| {
                q.is_empty()
                    || f.name.to_lowercase().contains(&q)
                    || f.host.to_lowercase().contains(&q)
                    || f.user.to_lowercase().contains(&q)
            };
            if self.settings.favorites.is_empty() {
                ui.add_space(16.0);
                ui.vertical_centered(|ui| {
                    ui.label(theme::weak("Noch keine Favoriten."));
                    ui.label(
                        RichText::new("„Neuer Favorit“ oder links verbinden und speichern.")
                            .small()
                            .color(theme::GREY),
                    );
                });
                return;
            }
            let mut select = None;
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for (i, f) in self.settings.favorites.iter().enumerate() {
                        if !matches(f) {
                            continue;
                        }
                        let live = self.running_of(&f.name);
                        let selected = self.selected_fav.as_deref() == Some(f.name.as_str());
                        let r = ui
                            .scope_builder(
                                egui::UiBuilder::new().sense(egui::Sense::click()),
                                |ui| {
                                    row_frame(selected).show(ui, |ui| {
                                        ui.set_width(ui.available_width());
                                        ui.horizontal(|ui| {
                                            let (color, tip) = reach_color(self.reach.get(&f.name));
                                            theme::dot(ui, color, 11.0).on_hover_text(tip);
                                            ui.vertical(|ui| {
                                                // leave room for the two buttons on the right
                                                ui.set_max_width(
                                                    (ui.available_width() - 100.0).max(80.0),
                                                );
                                                ui.spacing_mut().item_spacing.y = 2.0;
                                                ui.horizontal(|ui| {
                                                    let name_w = ui.available_width()
                                                        - if live.is_some() { 90.0 } else { 0.0 };
                                                    ui.scope(|ui| {
                                                        ui.set_max_width(name_w.max(60.0));
                                                        ui.add(
                                                            egui::Label::new(
                                                                RichText::new(&f.name)
                                                                    .strong()
                                                                    .size(15.5),
                                                            )
                                                            .truncate(),
                                                        );
                                                    });
                                                    if let Some(s) = live {
                                                        let (t, c) = state_short(s);
                                                        ui.label(RichText::new(t).small().color(c));
                                                    }
                                                });
                                                let sub = if f.user.is_empty() {
                                                    f.host.clone()
                                                } else {
                                                    format!("{} · {}", f.host, f.user)
                                                };
                                                ui.add(
                                                    egui::Label::new(theme::weak(sub).small())
                                                        .truncate(),
                                                );
                                                if let Some(t) = &f.last_connected {
                                                    ui.horizontal(|ui| {
                                                        let (r, _) = ui.allocate_exact_size(
                                                            vec2(13.0, 13.0),
                                                            egui::Sense::hover(),
                                                        );
                                                        icons::paint(
                                                            ui.painter(),
                                                            r,
                                                            Icon::Clock,
                                                            theme::WEAK,
                                                        );
                                                        ui.label(
                                                            theme::weak(format!("Zuletzt: {t}"))
                                                                .small(),
                                                        );
                                                    });
                                                }
                                            });
                                            ui.with_layout(
                                                Layout::right_to_left(Align::Center),
                                                |ui| {
                                                    if live.is_some() {
                                                        if theme::button(
                                                            ui,
                                                            Some(Icon::Unplug),
                                                            "",
                                                            Kind::Icon,
                                                            0.0,
                                                        )
                                                        .on_hover_text("Trennen")
                                                        .clicked()
                                                        {
                                                            acts.push(Act::Disconnect(
                                                                f.name.clone(),
                                                            ));
                                                        }
                                                    } else if theme::button(
                                                        ui,
                                                        Some(Icon::Plug),
                                                        "",
                                                        Kind::IconAccent,
                                                        0.0,
                                                    )
                                                    .on_hover_text("Verbinden")
                                                    .clicked()
                                                    {
                                                        acts.push(Act::Connect(i));
                                                    }
                                                    if theme::button(
                                                        ui,
                                                        Some(Icon::Pencil),
                                                        "",
                                                        Kind::Icon,
                                                        0.0,
                                                    )
                                                    .on_hover_text("Bearbeiten")
                                                    .clicked()
                                                    {
                                                        acts.push(Act::Edit(i));
                                                    }
                                                },
                                            );
                                        });
                                    })
                                },
                            )
                            .response;
                        if r.clicked() {
                            select = Some(f.name.clone());
                        }
                        if r.double_clicked() && live.is_none() {
                            acts.push(Act::Connect(i));
                        }
                        r.context_menu(|ui| {
                            if ui
                                .add_enabled(live.is_none(), egui::Button::new("Verbinden"))
                                .clicked()
                            {
                                acts.push(Act::Connect(i));
                            }
                            if ui.button("Bearbeiten …").clicked() {
                                acts.push(Act::Edit(i));
                            }
                            if ui.button("Duplizieren").clicked() {
                                acts.push(Act::Dup(i));
                            }
                            if ui.button("Gespeicherte Anmeldung verwerfen").clicked() {
                                acts.push(Act::Forget(i));
                            }
                            ui.separator();
                            if ui.button("Löschen …").clicked() {
                                acts.push(Act::Delete(i));
                            }
                        });
                        ui.add_space(6.0);
                    }
                });
            if select.is_some() {
                self.selected_fav = select;
            }
        });
    }

    fn apply_acts(&mut self, acts: Vec<Act>) {
        for a in acts {
            match a {
                Act::Connect(i) => {
                    if let Some(f) = self.settings.favorites.get(i).cloned() {
                        self.selected_fav = Some(f.name.clone());
                        self.connect(f, false);
                    }
                }
                Act::Edit(i) => {
                    if let Some(f) = self.settings.favorites.get(i).cloned() {
                        self.open_editor(Target::Favorite(Some(i)), f);
                    }
                }
                Act::Dup(i) => {
                    if let Some(mut d) = self.settings.favorites.get(i).cloned() {
                        d.name = format!("{} (Kopie)", d.name);
                        d.last_connected = None;
                        self.settings.favorites.insert(i + 1, d);
                        self.save();
                    }
                }
                Act::Forget(i) => {
                    if let Some(f) = self.settings.favorites.get(i).cloned() {
                        super::forget_token(&f);
                        self.message = Some(format!(
                            "Gespeicherte Anmeldung für „{}“ verworfen.",
                            f.name
                        ));
                    }
                }
                Act::Delete(i) => self.confirm_delete = Some(i),
                Act::Disconnect(n) => self.disconnect(&n),
                Act::Dismiss(n) => self.dismiss(&n),
                Act::EditSession(n) => {
                    if let Some(i) = self.settings.favorites.iter().position(|f| f.name == n) {
                        let f = self.settings.favorites[i].clone();
                        self.open_editor(Target::Favorite(Some(i)), f);
                    }
                }
                Act::ShowLog(p) => {
                    self.logview.open(&p);
                    self.page = Page::Log;
                }
                Act::KeepDrive(fav, drive) => {
                    self.offer_drive = None;
                    let defaults = self.settings.defaults.drives.clone();
                    if let Some(f) = self.settings.favorites.iter_mut().find(|f| f.name == fav) {
                        let list = f.overrides.drives.get_or_insert(defaults);
                        if !list.iter().any(|d| d.path == drive.path) {
                            list.push(drive);
                        }
                        self.save();
                        self.message = Some(format!("Laufwerk in „{fav}“ gespeichert."));
                    }
                }
                Act::SaveAdhoc(f) => {
                    self.offer_save = None;
                    self.open_editor(Target::Favorite(None), f);
                }
                Act::ConnectRecent(e) => {
                    match self
                        .settings
                        .favorites
                        .iter()
                        .find(|f| f.name == e.name)
                        .cloned()
                    {
                        Some(f) if !e.adhoc => self.connect(f, false),
                        _ => {
                            let mut f = Favorite::new(&e.host, &e.host, &e.user);
                            f.entra = e.entra;
                            f.domain = e.domain.clone();
                            self.connect(f, true);
                        }
                    }
                }
            }
        }
    }

    // ---- history page ----------------------------------------------------------------------

    fn history_page(&mut self, ui: &mut egui::Ui) {
        let mut acts = Vec::new();
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.set_min_height(ui.available_height() - 34.0);
            card_title(
                ui,
                Icon::Clock,
                "Verlauf",
                "Alle Verbindungen mit Dauer und Ergebnis",
            );
            ui.add_space(10.0);
            if self.history.is_empty() {
                ui.label(theme::weak("Noch keine Verbindungen."));
                return;
            }
            let entries: Vec<history::Entry> = self.history.iter().rev().cloned().collect();
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    egui::Grid::new("history")
                        .num_columns(6)
                        .striped(true)
                        .spacing([18.0, 8.0])
                        .min_col_width(60.0)
                        .show(ui, |ui| {
                            for h in ["Beginn", "Verbindung", "Benutzer", "Dauer", "Ergebnis", ""] {
                                ui.label(RichText::new(h).small().color(theme::WEAK));
                            }
                            ui.end_row();
                            for e in entries {
                                ui.label(&e.started);
                                ui.vertical(|ui| {
                                    ui.spacing_mut().item_spacing.y = 0.0;
                                    ui.add(
                                        egui::Label::new(RichText::new(&e.name).strong()).extend(),
                                    );
                                    if e.name != e.host {
                                        ui.add(
                                            egui::Label::new(theme::weak(&e.host).small()).extend(),
                                        );
                                    }
                                });
                                ui.add(egui::Label::new(theme::weak(&e.user)).extend());
                                ui.label(duration(&e.started, e.ended.as_deref()));
                                let (txt, col) = if e.ended.is_none() {
                                    if self.sessions.iter().any(|s| {
                                        s.running() && s.log.display().to_string() == e.log
                                    }) {
                                        ("läuft".to_string(), theme::GREEN)
                                    } else {
                                        ("unbekannt".to_string(), theme::GREY)
                                    }
                                } else if e.connected
                                    && !e.result.starts_with("Getrennt: connection failed")
                                {
                                    (short_result(&e.result), theme::TEXT)
                                } else {
                                    (short_result(&e.result), theme::RED)
                                };
                                ui.add(egui::Label::new(RichText::new(txt).color(col)).truncate());
                                ui.horizontal(|ui| {
                                    let path = std::path::PathBuf::from(&e.log);
                                    if path.exists()
                                        && theme::button(ui, Some(Icon::File), "", Kind::Icon, 0.0)
                                            .on_hover_text("Log anzeigen")
                                            .clicked()
                                    {
                                        acts.push(Act::ShowLog(path));
                                    }
                                    if theme::button(
                                        ui,
                                        Some(Icon::Plug),
                                        "",
                                        Kind::IconAccent,
                                        0.0,
                                    )
                                    .on_hover_text("Erneut verbinden")
                                    .clicked()
                                    {
                                        acts.push(Act::ConnectRecent(e.clone()));
                                    }
                                });
                                ui.end_row();
                            }
                        });
                });
        });
        if !acts.is_empty() {
            self.page = Page::Connect;
        }
        let show_log = acts.iter().any(|a| matches!(a, Act::ShowLog(_)));
        self.apply_acts(acts);
        if show_log {
            self.page = Page::Log;
        }
    }

    // ---- log page --------------------------------------------------------------------------

    /// Opens the newest log if none is selected.
    pub(super) fn log_latest_if_none(&mut self) {
        if self.logview.file.is_none()
            && let Some((p, _, _)) = logview::files(&crate::paths::log_dir()).into_iter().next()
        {
            self.logview.open(&p);
        }
    }

    fn log_page(&mut self, ui: &mut egui::Ui) {
        self.log_latest_if_none();
        self.logview.refresh();
        let files = logview::files(&crate::paths::log_dir());
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.set_min_height(ui.available_height() - 34.0);
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(vec2(26.0, 26.0), egui::Sense::hover());
                icons::paint(ui.painter(), r, Icon::File, theme::TEXT);
                ui.label(RichText::new("Log").heading());
                ui.add_space(12.0);
                let cur = self.logview.file.clone();
                let label_of = |p: &std::path::Path| {
                    files
                        .iter()
                        .find(|f| f.0 == p)
                        .map(|f| {
                            let live = self.sessions.iter().any(|s| s.running() && s.log == f.0);
                            format!("{} – {}{}", f.1, f.2, if live { "  (läuft)" } else { "" })
                        })
                        .unwrap_or_else(|| p.display().to_string())
                };
                let mut pick = None;
                egui::ComboBox::from_id_salt("logfile")
                    .width(320.0)
                    .selected_text(
                        cur.as_deref()
                            .map(label_of)
                            .unwrap_or_else(|| "kein Log".into()),
                    )
                    .show_ui(ui, |ui| {
                        for f in &files {
                            if ui
                                .selectable_label(
                                    cur.as_deref() == Some(f.0.as_path()),
                                    label_of(&f.0),
                                )
                                .clicked()
                            {
                                pick = Some(f.0.clone());
                            }
                        }
                    });
                if let Some(p) = pick {
                    self.logview.open(&p);
                }
                let (e, w) = self.logview.counts();
                if e > 0 {
                    ui.label(RichText::new(format!("{e} Fehler")).color(theme::RED));
                }
                if w > 0 {
                    ui.label(RichText::new(format!("{w} Warnungen")).color(theme::YELLOW));
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if theme::button(ui, Some(Icon::Folder), "Ordner", Kind::Secondary, 0.0)
                        .on_hover_text(crate::paths::log_dir().display().to_string())
                        .clicked()
                    {
                        let _ = std::fs::create_dir_all(crate::paths::log_dir());
                        let _ = std::process::Command::new("xdg-open")
                            .arg(crate::paths::log_dir())
                            .spawn();
                    }
                });
            });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                theme::field(
                    ui,
                    Icon::Search,
                    &mut self.logview.filter,
                    "Filtern …",
                    260.0,
                    true,
                    None,
                );
                ui.add_space(8.0);
                ui.checkbox(&mut self.logview.show_info, "Info");
                ui.checkbox(&mut self.logview.show_detail, "Details")
                    .on_hover_text("FreeRDP-Details und Statistikzeilen");
                ui.checkbox(&mut self.logview.follow, "Mitlaufen");
            });
            ui.add_space(6.0);
            egui::Frame::new()
                .fill(theme::INPUT)
                .stroke(egui::Stroke::new(1.0, theme::BORDER))
                .corner_radius(8)
                .inner_margin(egui::Margin::same(10))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.set_min_height(ui.available_height() - 12.0);
                    self.logview.body(ui);
                });
        });
        if self.active() {
            ui.ctx().request_repaint_after(Duration::from_millis(500));
        }
    }

    // ---- settings / favorite editor -------------------------------------------------------

    pub(super) fn editor_page(&mut self, ui: &mut egui::Ui) {
        let Some(mut ed) = self.editor.take() else {
            return;
        };
        let defaults = ed.target == Target::Defaults;
        let mut done: Option<bool> = None; // Some(true) = save
        let title = match ed.target {
            Target::Defaults => "Einstellungen".to_string(),
            Target::Favorite(None) => "Neuer Favorit".to_string(),
            Target::Favorite(Some(_)) => format!("Favorit „{}“", ed.fav.name),
        };
        let outputs = self.outputs.clone();
        let cats = super::categories(&ed.target);
        theme::card().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.set_min_height(ui.available_height() - 34.0);
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(vec2(26.0, 26.0), egui::Sense::hover());
                icons::paint(
                    ui.painter(),
                    r,
                    if defaults { Icon::Gear } else { Icon::Star },
                    theme::TEXT,
                );
                ui.label(RichText::new(&title).heading());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if defaults {
                        ui.label(theme::weak("Änderungen werden sofort gespeichert.").small());
                    } else {
                        if theme::button(ui, None, "Abbrechen", Kind::Secondary, 0.0).clicked() {
                            done = Some(false);
                        }
                        if theme::button(ui, None, "Speichern", Kind::Primary, 110.0).clicked() {
                            done = Some(true);
                        }
                    }
                });
            });
            if let Some(e) = &ed.error {
                ui.colored_label(theme::RED, e);
            }
            ui.add_space(8.0);
            let h = ui.available_height();
            ui.horizontal_top(|ui| {
                ui.allocate_ui_with_layout(vec2(180.0, h), Layout::top_down(Align::Min), |ui| {
                    for (i, c) in cats.iter().enumerate() {
                        if category_button(ui, c, ed.category == i).clicked() {
                            ed.category = i;
                        }
                    }
                    if !defaults {
                        ui.add_space(12.0);
                        ui.label(
                            RichText::new(
                                "„Standard verwenden“ übernimmt den Wert aus den Einstellungen.",
                            )
                            .small()
                            .color(theme::GREY),
                        );
                    }
                });
                ui.add_space(12.0);
                ui.vertical(|ui| {
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            let cat = cats.get(ed.category).copied().unwrap_or(cats[0]);
                            super::editor_ui(ui, &mut ed, cat, &outputs);
                        });
                });
            });
        });
        if defaults {
            // the settings page applies every change right away
            let changed = ed.defaults != self.settings.defaults
                || Some(ed.close_to_tray) != self.settings.close_to_tray;
            if changed && self.apply_editor(&ed).is_ok() {
                self.save();
            }
            if self.page == Page::Settings {
                self.editor = Some(ed);
            }
            return;
        }
        match done {
            Some(true) => {
                if let Err(e) = self.apply_editor(&ed) {
                    ed.error = Some(e);
                    self.editor = Some(ed);
                } else {
                    self.save();
                }
            }
            Some(false) => {}
            None => self.editor = Some(ed),
        }
    }

    // ---- footer ----------------------------------------------------------------------------

    fn footer(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let running_n = self.sessions.iter().filter(|s| s.running()).count();
        let single: Option<(String, bool)> = match running_n {
            1 => self
                .sessions
                .iter()
                .find(|s| s.running())
                .map(|s| (s.fav.name.clone(), s.state == State::Starting)),
            _ => None,
        };
        let active = running_n > 0;
        ui.horizontal(|ui| {
            let running: Vec<&Running> = self.sessions.iter().filter(|s| s.running()).collect();
            match running.as_slice() {
                [] => {
                    theme::dot(ui, None, 11.0);
                    ui.label(theme::weak("Keine Verbindung"));
                }
                [s] => {
                    let (t, c) = state_text(s);
                    theme::dot(ui, Some(c), 11.0);
                    ui.label(RichText::new(format!("{}:", s.fav.name)).strong());
                    ui.label(t);
                    if matches!(s.state, State::Starting | State::Reconnecting) {
                        ui.spinner();
                    }
                }
                v => {
                    theme::dot(ui, Some(theme::GREEN), 11.0);
                    let names: Vec<&str> = v.iter().map(|s| s.fav.name.as_str()).collect();
                    ui.label(format!("{} Verbindungen: {}", v.len(), names.join(", ")));
                }
            }
            if let Some(m) = &self.message {
                ui.add_space(12.0);
                // the buttons on the right need about 420 px
                let w = (ui.available_width() - 420.0).max(80.0);
                ui.allocate_ui_with_layout(
                    vec2(w, 24.0),
                    Layout::left_to_right(Align::Center),
                    |ui| {
                        ui.add(
                            egui::Label::new(RichText::new(m).italics().color(theme::WEAK))
                                .truncate(),
                        );
                    },
                );
            }
            let mut quit = false;
            let mut stop = None;
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let label = if active {
                    "Beenden (Sitzung läuft weiter)"
                } else {
                    "Beenden"
                };
                if theme::button(ui, Some(Icon::Exit), label, Kind::Secondary, 0.0).clicked() {
                    quit = true;
                }
                if let Some((n, starting)) = &single {
                    let starting = *starting;
                    if theme::button(
                        ui,
                        Some(Icon::Unplug),
                        if starting { "Abbrechen" } else { "Trennen" },
                        Kind::Secondary,
                        0.0,
                    )
                    .clicked()
                    {
                        stop = Some(n.clone());
                    }
                }
            });
            if quit {
                self.quit = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            if let Some(n) = stop {
                self.disconnect(&n);
            }
        });
    }
}

// ---- pieces ---------------------------------------------------------------------------------

fn nav_button(ui: &mut egui::Ui, icon: Icon, label: &str, selected: bool) -> egui::Response {
    let font = egui::TextStyle::Body.resolve(ui.style());
    let g = ui
        .painter()
        .layout_no_wrap(label.to_string(), font.clone(), theme::TEXT);
    let w = g.size().x + 18.0 + 10.0 + 32.0;
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 40.0), egui::Sense::click());
    let p = ui.painter();
    let fg = if selected || resp.hovered() {
        theme::TEXT
    } else {
        theme::WEAK
    };
    if selected {
        p.rect_filled(rect, 8.0, egui::Color32::from_rgb(28, 74, 140));
    } else if resp.hovered() {
        p.rect_filled(rect, 8.0, theme::CARD_HI);
    }
    let ir = egui::Rect::from_center_size(
        egui::pos2(rect.left() + 25.0, rect.center().y),
        vec2(19.0, 19.0),
    );
    icons::paint(p, ir, icon, fg);
    p.galley_with_override_text_color(
        egui::pos2(rect.left() + 44.0, rect.center().y - g.size().y / 2.0),
        g,
        fg,
    );
    if selected {
        p.rect_filled(
            egui::Rect::from_min_max(
                egui::pos2(rect.left(), rect.bottom() + 6.0),
                egui::pos2(rect.right(), rect.bottom() + 9.0),
            ),
            1.0,
            theme::ACCENT,
        );
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn category_button(ui: &mut egui::Ui, label: &str, selected: bool) -> egui::Response {
    let (rect, resp) =
        ui.allocate_exact_size(vec2(ui.available_width(), 34.0), egui::Sense::click());
    let p = ui.painter();
    if selected {
        p.rect_filled(rect, 7.0, egui::Color32::from_rgb(28, 74, 140));
    } else if resp.hovered() {
        p.rect_filled(rect, 7.0, theme::CARD_HI);
    }
    p.text(
        egui::pos2(rect.left() + 12.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        egui::TextStyle::Body.resolve(ui.style()),
        if selected || resp.hovered() {
            theme::TEXT
        } else {
            theme::WEAK
        },
    );
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn card_title(ui: &mut egui::Ui, icon: Icon, title: &str, sub: &str) {
    ui.horizontal(|ui| {
        let (r, _) = ui.allocate_exact_size(vec2(34.0, 34.0), egui::Sense::hover());
        icons::paint(ui.painter(), r, icon, theme::TEXT);
        ui.add_space(4.0);
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.label(RichText::new(title).heading());
            ui.label(theme::weak(sub));
        });
    });
    ui.add_space(4.0);
    let r = ui.max_rect();
    ui.painter().hline(
        r.left()..=r.right(),
        ui.cursor().top(),
        egui::Stroke::new(1.0, theme::BORDER),
    );
}

fn label_w(ui: &mut egui::Ui, text: &str, w: f32) {
    ui.add_sized([w, 36.0], egui::Label::new(text));
}

fn row_frame(selected: bool) -> egui::Frame {
    egui::Frame::new()
        .fill(if selected {
            egui::Color32::from_rgb(24, 44, 76)
        } else {
            theme::CARD_HI
        })
        .stroke(egui::Stroke::new(
            if selected { 1.5 } else { 1.0 },
            if selected {
                theme::ACCENT
            } else {
                theme::BORDER
            },
        ))
        .corner_radius(9)
        .inner_margin(egui::Margin::symmetric(12, 10))
}

/// State of a session as text and colour.
fn state_text(s: &Running) -> (String, egui::Color32) {
    match &s.state {
        State::Starting => ("Anmeldung und Verbindungsaufbau …".into(), theme::YELLOW),
        State::Connected => ("verbunden".into(), theme::GREEN),
        State::Reconnecting => ("unterbrochen, neuer Versuch …".into(), theme::YELLOW),
        State::Ended(m) => (m.clone(), theme::GREY),
    }
}

/// Short state for lists.
fn state_short(s: &Running) -> (&'static str, egui::Color32) {
    match &s.state {
        State::Starting => ("verbindet …", theme::YELLOW),
        State::Connected => ("verbunden", theme::GREEN),
        State::Reconnecting => ("unterbrochen …", theme::YELLOW),
        State::Ended(_) => ("beendet", theme::GREY),
    }
}

/// One session in detail (mockup "Aktuelle Verbindung").
fn session_big(ui: &mut egui::Ui, s: &Running, acts: &mut Vec<Act>) {
    let (state, color) = state_text(s);
    // Fixed column widths: long texts wrap or get shortened instead of widening the card.
    let full = ui.available_width();
    let pic_w = if full > 560.0 { 170.0 } else { 0.0 };
    let text_w = (full - pic_w - 16.0).max(160.0);
    ui.horizontal_top(|ui| {
        ui.allocate_ui_with_layout(vec2(text_w, 0.0), Layout::top_down(Align::Min), |ui| {
            ui.set_max_width(text_w);
            ui.horizontal(|ui| {
                theme::dot(ui, Some(color), 13.0);
                ui.add(egui::Label::new(RichText::new(&s.fav.name).size(22.0).strong()).truncate());
            });
            ui.add(
                egui::Label::new(theme::weak(if s.fav.user.is_empty() {
                    s.fav.host.clone()
                } else {
                    format!("{} · {}", s.fav.host, s.fav.user)
                }))
                .truncate(),
            );
            ui.horizontal_top(|ui| {
                if matches!(s.state, State::Starting | State::Reconnecting) {
                    ui.spinner();
                }
                ui.add(egui::Label::new(RichText::new(&state).color(color)).wrap());
            });
            if s.running() {
                ui.label(theme::weak(format!("seit {}", elapsed(s.started.elapsed()))).small());
            }
        });
        if pic_w > 0.0 {
            ui.add_space(16.0);
            let (r, _) = ui.allocate_exact_size(vec2(pic_w, 120.0), egui::Sense::hover());
            icons::screen(ui.painter(), r, s.state == State::Connected);
        }
    });
    ui.add_space(8.0);
    let r = ui.max_rect();
    ui.painter().hline(
        r.left()..=r.right(),
        ui.cursor().top(),
        egui::Stroke::new(1.0, theme::BORDER),
    );
    ui.add_space(10.0);
    let buttons_w = 180.0;
    let grid_w = (full - buttons_w - 16.0).max(200.0);
    ui.horizontal_top(|ui| {
        ui.allocate_ui_with_layout(vec2(grid_w, 0.0), Layout::top_down(Align::Min), |ui| {
            ui.set_max_width(grid_w);
            egui::Grid::new("session_details")
                .num_columns(3)
                .spacing([10.0, 8.0])
                .max_col_width(grid_w - 130.0)
                .show(ui, |ui| {
                    let rows = [
                        (Icon::Server, "Ziel / Host", s.fav.host.clone()),
                        (Icon::User, "Benutzer", s.fav.user.clone()),
                        (
                            Icon::Building,
                            "Domäne",
                            if s.fav.entra {
                                "— (Entra ID)".to_string()
                            } else if s.fav.domain.is_empty() {
                                "— (lokales Konto)".to_string()
                            } else {
                                s.fav.domain.clone()
                            },
                        ),
                    ];
                    let mut rows = rows.to_vec();
                    if let Some(g) = &s.gpu {
                        let short = g.split(", Darstellung").next().unwrap_or(g);
                        rows.push((Icon::Monitor, "Grafik", short.to_string()));
                    }
                    for (icon, k, v) in rows {
                        let (r, _) = ui.allocate_exact_size(vec2(20.0, 20.0), egui::Sense::hover());
                        icons::paint(ui.painter(), r, icon, theme::WEAK);
                        ui.label(theme::weak(k));
                        ui.add(egui::Label::new(v).truncate());
                        ui.end_row();
                    }
                });
        });
        ui.with_layout(Layout::top_down(Align::Max), |ui| {
            session_buttons(ui, s, acts, buttons_w);
        });
    });
}

/// One session as a compact row (several connections open).
fn session_row(ui: &mut egui::Ui, s: &Running, acts: &mut Vec<Act>) {
    let (state, color) = state_text(s);
    row_frame(false).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            theme::dot(ui, Some(color), 11.0);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                ui.set_max_width((ui.available_width() - 140.0).max(120.0));
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&s.fav.name).strong());
                    ui.add(egui::Label::new(RichText::new(&state).small().color(color)).truncate());
                });
                ui.add(
                    egui::Label::new(
                        theme::weak(format!("{} · {}", s.fav.host, s.fav.user)).small(),
                    )
                    .truncate(),
                );
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.horizontal(|ui| session_buttons_compact(ui, s, acts));
            });
        });
    });
}

fn session_buttons(ui: &mut egui::Ui, s: &Running, acts: &mut Vec<Act>, w: f32) {
    if !s.adhoc && theme::button(ui, Some(Icon::Pencil), "Bearbeiten", Kind::Secondary, w).clicked()
    {
        acts.push(Act::EditSession(s.fav.name.clone()));
    }
    if s.running() {
        let label = if s.state == State::Starting {
            "Abbrechen"
        } else {
            "Trennen"
        };
        if theme::button(ui, Some(Icon::Unplug), label, Kind::Secondary, w).clicked() {
            acts.push(Act::Disconnect(s.fav.name.clone()));
        }
    } else if theme::button(ui, Some(Icon::Close), "Entfernen", Kind::Secondary, w).clicked() {
        acts.push(Act::Dismiss(s.fav.name.clone()));
    }
    if theme::button(ui, Some(Icon::File), "Log anzeigen", Kind::Secondary, w).clicked() {
        acts.push(Act::ShowLog(s.log.clone()));
    }
}

fn session_buttons_compact(ui: &mut egui::Ui, s: &Running, acts: &mut Vec<Act>) {
    if s.running() {
        if theme::button(ui, Some(Icon::Unplug), "", Kind::Icon, 0.0)
            .on_hover_text("Trennen")
            .clicked()
        {
            acts.push(Act::Disconnect(s.fav.name.clone()));
        }
    } else if theme::button(ui, Some(Icon::Close), "", Kind::Icon, 0.0)
        .on_hover_text("Entfernen")
        .clicked()
    {
        acts.push(Act::Dismiss(s.fav.name.clone()));
    }
    if theme::button(ui, Some(Icon::File), "", Kind::Icon, 0.0)
        .on_hover_text("Log anzeigen")
        .clicked()
    {
        acts.push(Act::ShowLog(s.log.clone()));
    }
    if !s.adhoc
        && theme::button(ui, Some(Icon::Pencil), "", Kind::Icon, 0.0)
            .on_hover_text("Bearbeiten")
            .clicked()
    {
        acts.push(Act::EditSession(s.fav.name.clone()));
    }
}

fn elapsed(d: Duration) -> String {
    let m = d.as_secs() / 60;
    if m < 1 {
        "weniger als einer Minute".into()
    } else if m < 60 {
        format!("{m} min")
    } else {
        format!("{} h {:02} min", m / 60, m % 60)
    }
}

/// "YYYY-MM-DD HH:MM:SS" -> "DD.MM. HH:MM".
fn short_time(t: &str) -> String {
    if t.len() >= 16 {
        format!("{}.{}. {}", &t[8..10], &t[5..7], &t[11..16])
    } else {
        t.to_string()
    }
}

fn secs_of(t: &str) -> Option<i64> {
    // days are enough for durations within the history
    let d: i64 = t.get(8..10)?.parse().ok()?;
    let h: i64 = t.get(11..13)?.parse().ok()?;
    let m: i64 = t.get(14..16)?.parse().ok()?;
    let s: i64 = t.get(17..19).and_then(|v| v.parse().ok()).unwrap_or(0);
    Some(((d * 24 + h) * 60 + m) * 60 + s)
}

fn duration(start: &str, end: Option<&str>) -> String {
    match (secs_of(start), end.and_then(secs_of)) {
        (Some(a), Some(b)) if b >= a => {
            let s = b - a;
            if s < 60 {
                format!("{s} s")
            } else if s < 3600 {
                format!("{} min", s / 60)
            } else {
                format!("{} h {:02} min", s / 3600, (s % 3600) / 60)
            }
        }
        _ => "–".into(),
    }
}

fn short_result(r: &str) -> String {
    let r = r.strip_prefix("Getrennt: ").unwrap_or(r);
    let r = r.split(" (0x").next().unwrap_or(r);
    r.trim_end_matches('.').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times() {
        assert_eq!(short_time("2026-11-25 08:35:44"), "25.11. 08:35");
        assert_eq!(
            duration("2026-11-25 08:35:00", Some("2026-11-25 10:57:30")),
            "2 h 22 min"
        );
        assert_eq!(
            duration("2026-11-25 08:35:00", Some("2026-11-25 08:35:40")),
            "40 s"
        );
        assert_eq!(duration("2026-11-25 08:35:00", None), "–");
        assert_eq!(
            short_result("Getrennt: connection failed: Logon failed. (0x00020014)"),
            "connection failed: Logon failed"
        );
    }
}
