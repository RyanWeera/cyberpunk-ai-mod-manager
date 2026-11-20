//! Drawing the window: header, setup banner, rail, pages, status bar and dialogs. Every button calls a
//! `Window` method, so the logic stays testable without a screen.

use super::*;
use eframe::egui::{self, Align, Button, Color32, CornerRadius, Frame, Id, Key, Layout, Modifiers, ProgressBar, RichText, ScrollArea, Stroke, TextEdit, Ui, ViewportCommand, vec2};
use egui_extras::{Column, TableBuilder};

fn big(t: &Theme, text: &str) -> Button<'static> {
    Button::new(RichText::new(text.to_string()).color(t.ink).strong()).fill(t.accent)
}

fn outline(t: &Theme, text: &str) -> Button<'static> {
    Button::new(RichText::new(text.to_string()).color(t.accent)).fill(Color32::TRANSPARENT).stroke(Stroke::new(1.0, t.accent))
}

fn section(ui: &mut Ui, t: &Theme, text: &str) {
    ui.add_space(6.0);
    ui.label(RichText::new(text).color(t.second).size(16.0).strong());
}

fn dim(ui: &mut Ui, t: &Theme, text: &str) {
    ui.add(egui::Label::new(RichText::new(text).color(t.dim)).wrap());
}

fn card<R>(ui: &mut Ui, t: &Theme, add: impl FnOnce(&mut Ui) -> R) -> R {
    Frame::new().fill(t.panel).corner_radius(8).inner_margin(14).stroke(Stroke::new(1.0, t.panel2)).show(ui, add).inner
}

fn chip(ui: &mut Ui, t: &Theme, text: &str, color: Color32) {
    Frame::new().fill(t.panel2).corner_radius(10).inner_margin(egui::Margin::symmetric(8, 2)).show(ui, |ui| {
        ui.label(RichText::new(text).color(color).small());
    });
}

fn avatar_box(ui: &mut Ui, t: &Theme, text: &str) {
    Frame::new().fill(t.accent).corner_radius(6).inner_margin(egui::Margin::symmetric(10, 8)).show(ui, |ui| {
        ui.label(RichText::new(text).color(t.ink).strong().size(16.0));
    });
}

fn search_box(ui: &mut Ui, text: &mut String, hint: &str, width: f32) -> egui::Response {
    ui.add(TextEdit::singleline(text).hint_text(format!("\u{1f50d}  {hint}")).desired_width(width))
}

impl eframe::App for Window {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.step(ctx);
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.draw(ui);
    }
}

impl Window {
    /// The work of each frame before drawing: style, events, the close flow, raising and the clipboard.
    pub fn step(&mut self, ctx: &egui::Context) {
        if self.applied_theme != self.theme_name {
            theme::apply(ctx, self.theme());
            self.applied_theme = self.theme_name.clone();
        }
        self.tick();
        if ctx.input(|i| i.viewport().close_requested()) && !self.closed {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            self.close();
        }
        if self.closing && self.close_when_free() {
            ctx.send_viewport_cmd(ViewportCommand::Close);
        }
        if std::mem::take(&mut self.raise) {
            ctx.send_viewport_cmd(ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(ViewportCommand::Focus);
        }
        if let Some(text) = self.copied.take() {
            ctx.copy_text(text);
        }
        ctx.request_repaint_after(Duration::from_millis(150));
    }

    /// Work of each frame without drawing: events, a store copy that waited, the search pause.
    pub fn tick(&mut self) {
        self.drain();
        if self.stale {
            self.try_sync();
        }
        if self.browse.soon.is_some_and(|t| Instant::now() >= t) {
            self.browse_go(0);
        }
    }

    pub fn draw(&mut self, ui: &mut Ui) {
        let t = self.theme();
        let closing = self.closing;
        ui.add_enabled_ui(!closing, |ui| {
            self.header(ui, t);
            self.banners(ui, t);
            self.status_bar(ui, t);
            self.rail(ui, t);
            egui::CentralPanel::default().frame(Frame::new().fill(t.bg).inner_margin(18)).show(ui, |ui| {
                ScrollArea::vertical().id_salt("page").auto_shrink([false, false]).show(ui, |ui| match self.page {
                    Page::Browse => self.page_browse(ui, t),
                    Page::Mods => self.page_mods(ui, t),
                    Page::MyPacks => self.page_packs(ui, t),
                    Page::Add => self.page_add(ui, t),
                    Page::Find => self.page_find(ui, t),
                    Page::Downloads => self.page_downloads(ui, t),
                    Page::Crash => self.page_crash(ui, t),
                    Page::Ai => self.page_ai(ui, t),
                    Page::Settings => self.page_settings(ui, t),
                });
            });
        });
        if !closing {
            self.dialog(ui, t);
            self.keys(ui);
        }
    }

    fn keys(&mut self, ui: &Ui) {
        if self.page != Page::Mods || ui.ctx().egui_wants_keyboard_input() {
            return;
        }
        let (all, up, down, del, space) = ui.input_mut(|i| {
            (
                i.consume_key(Modifiers::CTRL, Key::A),
                i.consume_key(Modifiers::ALT, Key::ArrowUp),
                i.consume_key(Modifiers::ALT, Key::ArrowDown),
                i.consume_key(Modifiers::NONE, Key::Delete),
                i.consume_key(Modifiers::NONE, Key::Space),
            )
        });
        if all {
            self.select_all();
        }
        if up {
            self.move_sel(-1);
        }
        if down {
            self.move_sel(1);
        }
        if del {
            self.remove();
        }
        if space {
            self.toggle();
        }
    }

    fn header(&mut self, ui: &mut Ui, t: &'static Theme) {
        egui::Panel::top("header").frame(Frame::new().fill(t.panel).inner_margin(egui::Margin::symmetric(18, 12))).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("CP2077 AI Mod Manager").color(t.accent).strong().size(18.0));
                ui.label(RichText::new(format!("›  {}", self.page_title())).color(t.fg).size(18.0));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.add(big(t, "▶  Launch game")).on_hover_text("Copies your mods into the game, starts Cyberpunk 2077 and watches it for crashes.").clicked() {
                        self.launch();
                    }
                    let values = self.pack_values();
                    egui::ComboBox::from_id_salt("pack_pick")
                        .selected_text(self.pack_pick.clone())
                        .width(200.0)
                        .show_ui(ui, |ui| {
                            for v in values {
                                ui.selectable_value(&mut self.pack_pick, v.clone(), v);
                            }
                        })
                        .response
                        .on_hover_text("The mod pack to launch. 'Current mod list' launches the mods as the Mods tab shows them.");
                    ui.label(RichText::new("Mod pack").color(t.dim));
                    let (text, c) = if self.game_running { ("● Game running", OK) } else { ("● Game not running", t.dim) };
                    chip(ui, t, text, c);
                });
            });
        });
    }

    fn banners(&mut self, ui: &mut Ui, t: &'static Theme) {
        if let Some(text) = self.setup_banner() {
            egui::Panel::top("setup").frame(Frame::new().fill(t.banner).inner_margin(egui::Margin::symmetric(18, 8))).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(text).color(t.fg));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.small_button("✕").clicked() {
                            self.banner_hidden = true;
                        }
                        if ui.add(big(t, "Open Settings")).clicked() {
                            self.select(Page::Settings);
                        }
                    });
                });
            });
        }
        if let Some((text, btn, act)) = self.dl_banner.clone() {
            egui::Panel::top("dl_banner").frame(Frame::new().fill(t.banner).inner_margin(egui::Margin::symmetric(18, 8))).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.add(big(t, &btn)).clicked() {
                            self.banner_click(act);
                        }
                        ui.add(egui::Label::new(RichText::new(text).color(t.accent)).wrap());
                    });
                });
            });
        }
    }

    fn status_bar(&mut self, ui: &mut Ui, t: &'static Theme) {
        egui::Panel::bottom("status").frame(Frame::new().fill(t.panel).inner_margin(egui::Margin::symmetric(18, 6))).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add(egui::Label::new(RichText::new(self.status.clone()).color(t.dim)).truncate());
                if let Some((share, text)) = self.progress() {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.add(ProgressBar::new(share).desired_width(220.0).fill(t.accent));
                        ui.label(RichText::new(text).color(t.dim));
                    });
                }
            });
        });
    }

    fn rail(&mut self, ui: &mut Ui, t: &'static Theme) {
        egui::Panel::left("rail").exact_size(210.0).resizable(false).frame(Frame::new().fill(t.panel).inner_margin(egui::Margin::symmetric(10, 14))).show(ui, |ui| {
            let item = |ui: &mut Ui, w: &mut Window, p: Page, glyph: &str| {
                let on = w.page == p;
                let badge = w.badge(p);
                let fill = if on { t.sel } else { Color32::TRANSPARENT };
                let r = Frame::new().fill(fill).corner_radius(6).inner_margin(egui::Margin::symmetric(10, 7)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        let c = if on { t.accent } else { t.fg };
                        ui.label(RichText::new(glyph).font(theme::icon_font(16.0)).color(c));
                        ui.label(RichText::new(p.name()).color(c));
                        if badge > 0 {
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| chip(ui, t, &badge.to_string(), t.accent));
                        }
                    });
                });
                let r = ui.interact(r.response.rect, ui.scope_id().with(("rail", p.name())), egui::Sense::click());
                let tip = if p == Page::Add { "Add mods".to_string() } else { w.tab_text(p) };
                if r.on_hover_text(tip).clicked() {
                    w.rail_click(p);
                }
            };
            for entry in RAIL {
                match entry {
                    Some((p, g)) => item(ui, self, p, g),
                    None => {
                        ui.separator();
                    }
                }
            }
            ui.with_layout(Layout::bottom_up(Align::Min), |ui| item(ui, self, Page::Settings, "\u{e713}"));
        });
    }

    // ---------- pages ----------
    fn page_browse(&mut self, ui: &mut Ui, t: &'static Theme) {
        ui.horizontal(|ui| {
            for (text, kind, tip) in [("Mods", "mods", "Single mods from Nexus Mods."), ("Mod packs", "packs", "Nexus collections: ready-made packs of many mods that work together.")] {
                if ui.selectable_label(self.browse.kind == kind, text).on_hover_text(tip).clicked() && self.browse.kind != kind {
                    self.browse_kind(kind);
                }
            }
            let hint = self.browse_hint();
            let r = search_box(ui, &mut self.browse.q, hint, ui.available_width()).on_hover_text("Search Nexus Mods by name.");
            if r.changed() {
                self.browse_soon();
            }
            if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                self.browse_go(0);
            }
        });
        ui.horizontal(|ui| {
            ui.label("Sort by:");
            let before = (self.browse.sort, self.browse.view, self.browse.adult);
            egui::ComboBox::from_id_salt("sort").selected_text(nexus::SORTS[self.browse.sort].0).show_ui(ui, |ui| {
                for (i, (label, _)) in nexus::SORTS.iter().enumerate() {
                    ui.selectable_value(&mut self.browse.sort, i, *label);
                }
            });
            ui.label("View:");
            egui::ComboBox::from_id_salt("view").selected_text(VIEWS[self.browse.view].to_string()).show_ui(ui, |ui| {
                for (i, n) in VIEWS.iter().enumerate() {
                    ui.selectable_value(&mut self.browse.view, i, n.to_string());
                }
            });
            ui.checkbox(&mut self.browse.adult, "Show adult mods");
            if before != (self.browse.sort, self.browse.view, self.browse.adult) {
                self.browse_go(0);
            }
            let (text, c) = self.browse.info.clone();
            ui.label(RichText::new(text).color(c));
        });
        ui.add_space(6.0);
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width(220.0);
                section(ui, t, "Categories");
                let mut changed = false;
                for (name, n, on) in &mut self.browse.cats {
                    changed |= ui.checkbox(on, format!("{name}  ({})", util::commas(*n))).changed();
                }
                if changed {
                    self.browse_go(0);
                }
            });
            ui.vertical(|ui| {
                for m in self.browse.mods.clone() {
                    self.browse_card(ui, t, &m);
                    ui.add_space(6.0);
                }
                self.pager_row(ui);
            });
        });
    }

    fn browse_card(&mut self, ui: &mut Ui, t: &'static Theme, m: &Value) {
        let pack = m.get("slug").is_some();
        card(ui, t, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_top(|ui| {
                let thumb = self.browse.thumbs.iter().find(|(u, _)| u == s(&m["thumb"])).map(|(_, p)| p.clone());
                let size = vec2(160.0, 90.0);
                match thumb {
                    Some(p) => {
                        ui.add(egui::Image::from_uri(super::util::file_uri(&p)).fit_to_exact_size(size).corner_radius(4));
                    }
                    None => {
                        let (r, _) = ui.allocate_exact_size(size, egui::Sense::hover());
                        ui.painter().rect_filled(r, CornerRadius::same(4), t.panel2);
                    }
                }
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        let what = if pack { "mod pack" } else { "mod" };
                        let name = RichText::new(s(&m["name"])).color(t.fg).strong().size(16.0);
                        if ui.link(name).on_hover_text(format!("Open the {what}'s page on Nexus Mods.")).clicked() {
                            self.open_url(&Self::browse_page_url(m));
                        }
                        if !s(&m["author"]).is_empty() {
                            ui.label(RichText::new(format!("  by {}", s(&m["author"]))).color(t.dim));
                        }
                    });
                    let sum: String = s(&m["summary"]).chars().take(220).collect();
                    if !sum.is_empty() {
                        dim(ui, t, &sum);
                    }
                    ui.horizontal(|ui| {
                        if !s(&m["category"]).is_empty() {
                            chip(ui, t, s(&m["category"]), t.second);
                        }
                        if pack {
                            ui.label(RichText::new(format!("{} mods", util::commas(m["mod_count"].as_i64().unwrap_or(0)))).color(t.dim));
                        }
                        for (key, glyph, tip) in [("downloads", "⬇", "downloads"), ("endorsements", "👍", "endorsements")] {
                            let n = m[key].as_f64().unwrap_or(0.0);
                            ui.label(RichText::new(format!("{glyph} {}", short(n))).color(t.dim)).on_hover_text(format!("{} {tip}", util::commas(n as i64)));
                        }
                        let when = ago(s(&m["updated"]), None);
                        if !when.is_empty() {
                            ui.label(RichText::new(format!("Updated {when}")).color(t.dim));
                        }
                    });
                });
                ui.with_layout(Layout::right_to_left(Align::Min), |ui| match self.mark(&m["id"]) {
                    Mark::Install => {
                        if ui.add(big(t, "+  Install")).clicked() {
                            self.browse_install(m);
                        }
                    }
                    Mark::Installed => {
                        ui.add_enabled(false, Button::new("✔  Installed"));
                    }
                    Mark::Added => {
                        ui.add_enabled(false, Button::new("Added"));
                    }
                });
            });
        });
    }

    fn pager_row(&mut self, ui: &mut Ui) {
        if self.browse.total == 0 {
            return;
        }
        let (page, last) = (self.browse.page, self.pages() - 1);
        ui.horizontal(|ui| {
            if ui.add_enabled(page > 0, Button::new("‹")).clicked() {
                self.browse_go(page - 1);
            }
            for p in self.pager() {
                match p {
                    Some(n) => {
                        if ui.selectable_label(n == page, util::commas(n + 1)).clicked() && n != page {
                            self.browse_go(n);
                        }
                    }
                    None => {
                        ui.label("…");
                    }
                }
            }
            if ui.add_enabled(page < last, Button::new("›")).clicked() {
                self.browse_go(page + 1);
            }
        });
    }

    fn page_mods(&mut self, ui: &mut Ui, t: &'static Theme) {
        ui.horizontal(|ui| {
            avatar_box(ui, t, &self.pack_avatar());
            ui.vertical(|ui| {
                ui.label(RichText::new(self.pack_pick.clone()).color(t.fg).strong().size(20.0));
                ui.label(RichText::new(self.pack_sub()).color(t.dim));
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.menu_button("•••", |ui| {
                    if ui.button("Save this list as a pack…").clicked() {
                        self.save_pack();
                    }
                    if ui.button("Use the picked pack").clicked() {
                        self.use_pack(None);
                    }
                    if ui.button("Delete the picked pack").clicked() {
                        self.delete_pack(None);
                    }
                    if ui.button("Open the mod's folder").clicked() {
                        self.open_folder();
                    }
                })
                .response
                .on_hover_text("Mod packs: save this list as a pack, use or delete the pack picked next to 'Launch game'.");
            });
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add(big(t, "+  Add mods")).on_hover_text("Browse Nexus Mods, paste links, add files from this PC, or install the core frameworks.").clicked() {
                    self.add_dialog();
                }
                if ui.add(outline(t, "Merge packs")).on_hover_text("Make one big pack from several packs.").clicked() {
                    self.merge_packs();
                }
                let w = ui.available_width();
                search_box(ui, &mut self.search, "Search mods…", w);
            });
        });
        ui.horizontal(|ui| {
            for name in ["All", "On", "Off"] {
                if ui.selectable_label(self.mods_show == name, name).clicked() {
                    self.mods_show = name;
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let tip = "Put the mods in the best load order: frameworks first, each mod below the mods it needs, small patches below the big mods they change. Then check for missing mods. Undo it in the Crash doctor tab.";
                if ui.link("⇅  Auto-sort").on_hover_text(tip).clicked() {
                    self.auto_sort();
                }
                if ui.link("Apply to game").on_hover_text("Copy the mods that are on into the game folder now.").clicked() {
                    self.redeploy();
                }
            });
        });
        ui.add_space(6.0);
        if let Some((title, text, button)) = self.mods_empty() {
            ui.vertical_centered(|ui| {
                ui.add_space(40.0);
                ui.label(RichText::new(title).color(t.fg).size(22.0).strong());
                dim(ui, t, text);
                if button && ui.add(big(t, "+  Add mods")).clicked() {
                    self.add_dialog();
                }
            });
            return;
        }
        let rows = self.mod_rows();
        let mut clicks: Vec<(String, bool, bool)> = vec![];
        let mut flips: Vec<(String, bool)> = vec![];
        let mut menu: Vec<(String, &'static str)> = vec![];
        let mods = ui.input(|i| i.modifiers);
        TableBuilder::new(ui)
            .id_salt("mods")
            .striped(true)
            .sense(egui::Sense::click())
            .column(Column::exact(60.0))
            .column(Column::remainder().at_least(260.0))
            .column(Column::exact(140.0))
            .column(Column::exact(130.0))
            .header(26.0, |mut h| {
                for c in ["On", "Mod", "Version", "Actions"] {
                    h.col(|ui| {
                        ui.label(RichText::new(c).color(t.dim));
                    });
                }
            })
            .body(|mut body| {
                for r in &rows {
                    body.row(46.0, |mut row| {
                        row.set_selected(self.sel.contains(&r.id));
                        row.col(|ui| {
                            let mut on = r.on;
                            if ui.checkbox(&mut on, "").on_hover_text("Turn on / off").changed() {
                                flips.push((r.id.clone(), on));
                            }
                        });
                        row.col(|ui| {
                            let c = if r.on { t.fg } else { t.dim };
                            ui.vertical(|ui| {
                                ui.label(RichText::new(&r.name).color(c));
                                ui.label(RichText::new(&r.sub).color(t.dim).small());
                            });
                        });
                        row.col(|ui| {
                            ui.label(RichText::new(&r.version).color(if r.on { t.fg } else { t.dim }));
                        });
                        row.col(|ui| {
                            ui.horizontal(|ui| {
                                if ui.small_button("⟳").on_hover_text("Update").clicked() {
                                    menu.push((r.id.clone(), "update"));
                                }
                                if ui.small_button("✕").on_hover_text("Remove").clicked() {
                                    menu.push((r.id.clone(), "remove"));
                                }
                                ui.menu_button("•••", |ui| {
                                    for (label, key) in [
                                        ("Turn on / off", "toggle"),
                                        ("Update", "update"),
                                        ("Open folder", "folder"),
                                        ("Move up    Alt+Up", "up"),
                                        ("Move down    Alt+Down", "down"),
                                        ("Remove", "remove"),
                                    ] {
                                        if ui.button(label).clicked() {
                                            menu.push((r.id.clone(), key));
                                        }
                                    }
                                });
                            });
                        });
                        let resp = row.response();
                        if resp.clicked() {
                            clicks.push((r.id.clone(), mods.ctrl, mods.shift));
                        }
                        if resp.double_clicked() {
                            menu.push((r.id.clone(), "toggle"));
                        }
                    });
                }
            });
        for (id, ctrl, shift) in clicks {
            self.click_row(&id, ctrl, shift);
        }
        for (id, on) in flips {
            self.set_one(&id, on);
        }
        for (id, key) in menu {
            if !self.sel.contains(&id) {
                self.sel = vec![id.clone()];
            }
            match key {
                "toggle" => self.toggle(),
                "update" => self.update(),
                "folder" => self.open_folder(),
                "up" => self.move_sel(-1),
                "down" => self.move_sel(1),
                _ => self.remove(),
            }
        }
    }

    fn page_packs(&mut self, ui: &mut Ui, t: &'static Theme) {
        ui.horizontal(|ui| {
            if ui.add(big(t, "+  Find mod packs")).on_hover_text("Browse the mod packs (Nexus collections) on Nexus Mods.").clicked() {
                self.find_packs();
            }
            if ui.add(outline(t, "Merge packs")).on_hover_text("Make one big pack from several packs.").clicked() {
                self.merge_packs();
            }
            if ui.add(outline(t, "Save my mod list as a pack")).on_hover_text("Keep the mods that are on now, in their order.").clicked() {
                self.save_pack();
            }
            let w = ui.available_width();
            search_box(ui, &mut self.packs_q, "Search my mod packs…", w);
        });
        ui.horizontal(|ui| {
            for (text, key) in [("All", "all"), ("From Nexus", "nexus"), ("My own", "own")] {
                if ui.selectable_label(self.packs_show == key, text).clicked() {
                    self.packs_show = key;
                }
            }
        });
        ui.add_space(6.0);
        let cards = self.pack_cards();
        if cards.is_empty() {
            dim(ui, t, self.packs_empty());
        }
        for c in cards {
            card(ui, t, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    avatar_box(ui, t, &c.avatar);
                    ui.vertical(|ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(&c.name).color(t.fg).strong().size(16.0));
                            chip(ui, t, c.chip, t.second);
                            if c.in_use {
                                chip(ui, t, "IN USE", t.accent);
                            }
                        });
                        ui.label(RichText::new(&c.sub).color(t.dim));
                    });
                });
                ui.horizontal_wrapped(|ui| {
                    if c.missing > 0 && ui.add(big(t, "Complete installation")).on_hover_text("Download and install this pack's mods that are not installed yet.").clicked() {
                        self.complete_pack(&c.name);
                    }
                    if ui.add(big(t, "Use")).on_hover_text("Turn on this pack's mods in its order and turn off the rest.").clicked() {
                        self.use_pack(Some(&c.name));
                    }
                    if ui.button("▶  Launch").on_hover_text("Start the game with this pack.").clicked() {
                        self.launch_pack(&c.name);
                    }
                    if ui.button(if c.open { "Hide mods" } else { "Show mods" }).on_hover_text("The mods in this pack, lowest priority first.").clicked() {
                        self.toggle_pack_open(&c.name);
                    }
                    if let Some(url) = &c.url
                        && ui.button("Nexus page").on_hover_text("Open this pack on Nexus Mods.").clicked()
                    {
                        self.open_url(url);
                    }
                    if ui.button("Delete").on_hover_text("Forget this pack. Its mods stay installed.").clicked() {
                        self.delete_pack(Some(&c.name));
                    }
                });
                if c.open {
                    if c.lines.is_empty() {
                        dim(ui, t, "This pack has no mods.");
                    }
                    for l in &c.lines {
                        ui.label(RichText::new(l).color(t.fg));
                    }
                }
            });
            ui.add_space(6.0);
        }
    }

    fn page_add(&mut self, ui: &mut Ui, t: &'static Theme) {
        section(ui, t, "Paste links, one per line");
        dim(ui, t, "Nexus mod pages, Nexus collections (mod packs), GitHub pages, or direct .zip / .7z / .rar links. I find each mod's required mods and frameworks myself.");
        ui.add(TextEdit::multiline(&mut self.links).hint_text(EXAMPLES).desired_rows(8).desired_width(f32::INFINITY));
        ui.horizontal(|ui| {
            if ui.add(big(t, "▶ Download and set up everything")).on_hover_text("Download, virus-scan, install and set up every link.").clicked() {
                self.add_links();
            }
            if ui.button("Paste").on_hover_text("Paste links from the clipboard.").clicked() {
                self.paste();
            }
            if ui.button("Add files from this PC…").on_hover_text("Install mod archives you already downloaded.").clicked() {
                self.add_files();
            }
            if ui.button("Install the core frameworks").on_hover_text("RED4ext, Cyber Engine Tweaks, redscript, ArchiveXL, TweakXL and Codeware.").clicked() {
                self.install_core();
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(RichText::new(self.link_count()).color(t.dim));
            });
        });
        ui.add_space(8.0);
        card(ui, t, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new("How downloads work").color(t.fg).strong());
            for line in [
                "•  Frameworks (RED4ext, CET, redscript, ArchiveXL, TweakXL, Codeware, Input Loader, Mod Settings) come from GitHub: no account, no clicks.",
                "•  Nexus with a free account: Nexus gives a file only after a click on 'Slow download'. I click it for you in a hidden browser after you log in to Nexus once (Settings can turn this off). When that fails, I open the file's page in your browser: click 'Slow download' there and I do the rest.",
                "•  Nexus Premium: no clicks at all.  •  Every download is virus-scanned first (you can turn that off in Settings).",
            ] {
                dim(ui, t, line);
            }
        });
    }

    fn page_find(&mut self, ui: &mut Ui, t: &'static Theme) {
        section(ui, t, "Paste a video link");
        dim(ui, t, "YouTube works best: I read the mod links, the title and the chapters in the description. TikTok, Instagram and other sites give me only the caption. You can also paste a caption or a mod name.");
        ui.horizontal(|ui| {
            let w = ui.available_width() - 150.0;
            let r = ui.add(TextEdit::singleline(&mut self.video).desired_width(w));
            let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            if ui.add(big(t, "▶ Find the mods")).clicked() || enter {
                self.find_video();
            }
        });
        let (text, c) = self.video_info.clone();
        ui.add(egui::Label::new(RichText::new(text).color(c)).wrap());
        ui.add_space(6.0);
        let rows = self.found.clone();
        let mut open = false;
        TableBuilder::new(ui)
            .id_salt("found")
            .striped(true)
            .sense(egui::Sense::click())
            .column(Column::remainder().at_least(240.0))
            .column(Column::exact(300.0))
            .column(Column::exact(120.0))
            .header(26.0, |mut h| {
                for c in ["Mod", "Found by", "Endorsements"] {
                    h.col(|ui| {
                        ui.label(RichText::new(c).color(t.dim));
                    });
                }
            })
            .body(|mut body| {
                for (url, name, why, e) in &rows {
                    body.row(28.0, |mut row| {
                        row.set_selected(self.found_sel.contains(url));
                        for text in [name, why, e] {
                            row.col(|ui| {
                                ui.label(text.as_str());
                            });
                        }
                        let r = row.response();
                        if r.clicked() {
                            match self.found_sel.iter().position(|u| u == url) {
                                Some(i) => {
                                    self.found_sel.remove(i);
                                }
                                None => self.found_sel.push(url.clone()),
                            }
                        }
                        if r.double_clicked() {
                            self.found_sel = vec![url.clone()];
                            open = true;
                        }
                    });
                }
            });
        if open {
            self.open_found();
        }
        ui.horizontal(|ui| {
            if ui.add(big(t, "▶ Add the selected mods")).on_hover_text("Download and install the selected mods, like links in the Add mods tab.").clicked() {
                self.add_found();
            }
            if ui.button("Open page").on_hover_text("Open the mod's page in your browser to check that it is the mod in the video. Shortcut: double-click.").clicked() {
                self.open_found();
            }
        });
    }

    fn page_downloads(&mut self, ui: &mut Ui, t: &'static Theme) {
        ui.horizontal(|ui| {
            if ui.button("Open Nexus page").on_hover_text("Open the Nexus page of the selected download again.").clicked() {
                self.job_reopen();
            }
            if ui.button("Skip").on_hover_text("Do not download the selected file.").clicked() {
                self.job_skip(false);
            }
            if ui.button("Retry").on_hover_text("Try the selected download again.").clicked() {
                self.job_skip(true);
            }
            if ui.button("Retry all failed").clicked() {
                self.retry_failed();
            }
            if ui.button("Clear finished").on_hover_text("Remove finished and failed rows from this list.").clicked() {
                self.clear_jobs();
            }
        });
        let rows = self.job_rows();
        let mut pick = None;
        TableBuilder::new(ui)
            .id_salt("jobs")
            .striped(true)
            .sense(egui::Sense::click())
            .min_scrolled_height(260.0)
            .column(Column::exact(320.0))
            .column(Column::remainder().at_least(240.0))
            .column(Column::exact(100.0))
            .header(26.0, |mut h| {
                for c in ["Mod", "Status", "Progress"] {
                    h.col(|ui| {
                        ui.label(RichText::new(c).color(t.dim));
                    });
                }
            })
            .body(|mut body| {
                for (id, name, status, progress, state) in &rows {
                    body.row(28.0, |mut row| {
                        row.set_selected(self.job_sel.as_ref() == Some(id));
                        let c = match state.as_str() {
                            "done" => OK,
                            "failed" => BAD,
                            "waiting" | "needkey" => t.accent,
                            _ => t.fg,
                        };
                        row.col(|ui| {
                            ui.label(name.as_str());
                        });
                        row.col(|ui| {
                            ui.label(RichText::new(status.as_str()).color(c));
                        });
                        row.col(|ui| {
                            ui.label(progress.as_str());
                        });
                        if row.response().clicked() {
                            pick = Some(id.clone());
                        }
                    });
                }
            });
        if pick.is_some() {
            self.job_sel = pick;
        }
        section(ui, t, "Messages");
        let mut log = self.log.join("\n");
        ui.add(TextEdit::multiline(&mut log).font(egui::TextStyle::Monospace).desired_rows(7).desired_width(f32::INFINITY).interactive(false));
    }

    fn page_crash(&mut self, ui: &mut Ui, t: &'static Theme) {
        card(ui, t, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(self.game_state()).color(if self.game_running { OK } else { t.fg }));
            if let Some(b) = self.bisect_text() {
                dim(ui, t, &b);
            }
        });
        ui.horizontal_wrapped(|ui| {
            if ui.add(big(t, "▶ Check my mods now")).on_hover_text("Look for missing frameworks, mod errors in the game logs and conflicts.").clicked() {
                self.act(Act::CheckNow);
            }
            let tip = "The AI on this PC reads your load order and the problems, then suggests fixes and mod moves. You see them before anything changes. Set it up in the AI helpers tab.";
            if ui.button("Ask the local AI").on_hover_text(tip).clicked() {
                self.ask_ai();
            }
            if ui.button("Find the bad mod for me").on_hover_text("Turns half the mods off for each game start until the mod that crashes the game is found.").clicked() {
                self.bisect_begin();
            }
            if ui.button("Stop the search").on_hover_text("Stop the bad-mod search and turn your mods back on.").clicked() {
                self.bisect_cancel();
            }
            let tip = "Save a text file about the last crash or freeze and your mods, to share with a mod author. Folder paths of this PC are hidden.";
            if ui.button("Save crash report").on_hover_text(tip).clicked() {
                self.save_report();
            }
        });
        ui.horizontal_wrapped(|ui| {
            let tip = "Copies the crash report as a question for ChatGPT, Claude or another AI chat. Nothing is sent anywhere; it only goes to your clipboard. Folder paths are hidden.";
            if ui.button("Copy for an AI").on_hover_text(tip).clicked() {
                self.copy_report();
            }
            if ui.button("Open saved crash logs").on_hover_text("After each crash I keep a zip of the crash report and the mod logs, because the game deletes old logs.").clicked() {
                self.open_crash_logs();
            }
        });
        ui.add_space(6.0);
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width(420.0);
                let mut pick = None;
                let probs = self.problems.clone();
                TableBuilder::new(ui)
                    .id_salt("problems")
                    .striped(true)
                    .sense(egui::Sense::click())
                    .column(Column::exact(90.0))
                    .column(Column::remainder())
                    .header(26.0, |mut h| {
                        for c in ["Level", "Problem"] {
                            h.col(|ui| {
                                ui.label(RichText::new(c).color(t.dim));
                            });
                        }
                    })
                    .body(|mut body| {
                        for p in &probs {
                            let id = s(&p["id"]).to_string();
                            body.row(28.0, |mut row| {
                                row.set_selected(self.prob_sel.as_deref() == Some(id.as_str()));
                                let sev = s(&p["severity"]);
                                let c = match sev {
                                    "high" => BAD,
                                    "medium" => WARN,
                                    _ => t.dim,
                                };
                                row.col(|ui| {
                                    ui.label(RichText::new(level(sev)).color(c));
                                });
                                row.col(|ui| {
                                    ui.label(s(&p["title"]));
                                });
                                if row.response().clicked() {
                                    pick = Some(id.clone());
                                }
                            });
                        }
                    });
                if pick.is_some() {
                    self.select_problem(pick);
                }
            });
            ui.vertical(|ui| {
                card(ui, t, |ui| {
                    ui.set_width(ui.available_width());
                    ui.add(egui::Label::new(RichText::new(self.problem_detail()).color(t.fg)).wrap());
                });
                ui.label(RichText::new("How do you want to fix it?").color(t.second));
                for (i, label) in self.fix_labels().into_iter().enumerate() {
                    ui.radio_value(&mut self.fix_choice, i, label);
                }
                if ui.add_enabled(self.problem().is_some(), big(t, "▶ Apply this fix")).clicked() {
                    self.apply_fix();
                }
            });
        });
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            section(ui, t, "Fixes done");
            ui.label(RichText::new("   Select one and press Undo to reverse it.").color(t.dim));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("Undo").on_hover_text("Reverse the selected fix.").clicked() {
                    self.undo();
                }
            });
        });
        let hist = self.history();
        let mut pick = None;
        TableBuilder::new(ui)
            .id_salt("history")
            .striped(true)
            .sense(egui::Sense::click())
            .column(Column::exact(140.0))
            .column(Column::remainder())
            .header(24.0, |mut h| {
                for c in ["When", "Fix"] {
                    h.col(|ui| {
                        ui.label(RichText::new(c).color(t.dim));
                    });
                }
            })
            .body(|mut body| {
                for (i, (when, what)) in hist.iter().enumerate() {
                    body.row(26.0, |mut row| {
                        row.set_selected(self.hist_sel == Some(i));
                        row.col(|ui| {
                            ui.label(when.as_str());
                        });
                        row.col(|ui| {
                            ui.label(what.as_str());
                        });
                        if row.response().clicked() {
                            pick = Some(i);
                        }
                    });
                }
            });
        if pick.is_some() {
            self.hist_sel = pick;
        }
    }

    fn page_ai(&mut self, ui: &mut Ui, t: &'static Theme) {
        section(ui, t, "Local AI (free, runs on this PC)");
        dim(ui, t, "An open AI model (Qwen3.5) comes with this app and runs on your PC: free, no account, no install, and offline. 'Ask the local AI' in the Crash doctor tab lets it read your load order and the problems I found, pick fixes, and find patches that must load after the mod they change. You see every change first, and you can undo it. It runs only while this window is open and the game is not running: it stops when the game starts or the window closes. A graphics card makes it fast; it also runs on the processor, more slowly.");
        ui.horizontal(|ui| {
            if ui.add(big(t, "▶ Check local AI")).on_hover_text("Starts the local AI once to check it. When you run the app from its source code, this first downloads the AI (about 1.3 GB).").clicked() {
                self.setup_ai();
            }
            if ui.button("Ask the local AI").on_hover_text("Same as the button in the Crash doctor tab.").clicked() {
                self.ask_ai();
            }
        });
        let (text, c) = self.ai_info.clone();
        ui.add(egui::Label::new(RichText::new(text).color(c)).wrap());
        ui.add_space(14.0);
        section(ui, t, "Tell the local AI about a bug");
        dim(ui, t, "Write what goes wrong in the game, for example: 'My modded car does not show up'. The local AI reads your mods and the problems I found, tells you the most likely cause, and offers fixes. You see every change first.");
        ui.add(TextEdit::multiline(&mut self.bug_text).desired_rows(3).desired_width(f32::INFINITY));
        if ui.add(big(t, "▶ Ask about this bug")).on_hover_text("The local AI explains the bug and offers fixes.").clicked() {
            self.ask_bug();
        }
        ui.add_space(14.0);
        section(ui, t, "Claude and other AI apps");
        dim(ui, t, "Give a mod link or a video link to Claude and ask it to set the mods up: it adds them here, sorts the load order and checks for problems. This works in AI apps that support MCP, such as Claude Desktop, Claude Code and Cursor. The AI uses this window, so you see what it does. AI apps add only Nexus Mods links and the core frameworks: paste other links in the Add mods tab yourself. With a free Nexus account the 'Slow download' clicks work as in the Add mods tab.");
        ui.horizontal(|ui| {
            if ui.button("Copy setup for Claude Desktop").on_hover_text("Copies the MCP setup text for Claude Desktop and similar apps.").clicked() {
                self.copy_setup(true);
            }
            if ui.button("Copy setup for Claude Code").on_hover_text("Copies the command that adds this app to Claude Code.").clicked() {
                self.copy_setup(false);
            }
        });
        dim(ui, t, "Claude Desktop: open Settings > Developer > Edit Config, paste the text into claude_desktop_config.json (if the file already has \"mcpServers\", add only the cp2077-ai-mod-manager part), save, and restart Claude. Other apps: paste it where the app adds MCP servers.\nClaude Code: paste the command in a terminal.\nIf you move this app, copy the setup again.");
    }

    fn page_settings(&mut self, ui: &mut Ui, t: &'static Theme) {
        section(ui, t, "Game");
        egui::Grid::new("settings_game").num_columns(3).spacing(vec2(10.0, 8.0)).show(ui, |ui| {
            ui.label("Cyberpunk 2077 folder");
            let r = ui.add(TextEdit::singleline(&mut self.game_var).desired_width(520.0));
            if r.lost_focus() {
                self.save_settings();
            }
            ui.horizontal(|ui| {
                if ui.button("Browse…").clicked() {
                    self.browse_game();
                }
                if ui.button("Find it for me").on_hover_text("Look in Steam, GOG and Epic.").clicked() {
                    self.detect_game();
                }
            });
            ui.end_row();
            ui.label("");
            let (text, c) = self.game_info();
            ui.label(RichText::new(text).color(c));
            ui.end_row();
        });
        section(ui, t, "Nexus Mods");
        egui::Grid::new("settings_nexus").num_columns(3).spacing(vec2(10.0, 8.0)).show(ui, |ui| {
            ui.label("API key");
            let r = ui.add(TextEdit::singleline(&mut self.key_var).password(!self.show_key).desired_width(520.0));
            if r.lost_focus() {
                self.save_settings();
            }
            ui.horizontal(|ui| {
                if ui.button("Check key").clicked() {
                    self.check_key();
                }
                if ui.button("Get my key").on_hover_text("Opens your Nexus account page. Copy the personal API key from there.").clicked() {
                    self.open_url(KEY_PAGE);
                }
            });
            ui.end_row();
            ui.label("");
            ui.checkbox(&mut self.show_key, "Show");
            ui.end_row();
            ui.label("");
            let (text, c) = self.key_info.clone();
            ui.label(RichText::new(text).color(c));
            ui.end_row();
            ui.label("Download button");
            let (text, c) = self.nxm_info();
            ui.label(RichText::new(text).color(c));
            let (label, main) = self.nxm_btn();
            let b = if main { big(t, label) } else { Button::new(label) };
            if ui.add(b).on_hover_text("Makes the 'Mod Manager Download' and 'Slow download' buttons on Nexus send files to this app.").clicked() {
                self.register_nxm();
            }
            ui.end_row();
        });
        section(ui, t, "Options");
        let st = self.v.settings.clone();
        for (key, on, text) in [
            ("virus_scan", st.virus_scan, "Scan every download for viruses with Microsoft Defender (turn off to go faster with many mods)"),
            ("auto_fix", st.auto_fix, "After a crash, fix a problem by myself when it has only one fix"),
            ("include_optional", st.include_optional, "Tick the optional mods of a mod pack at first"),
        ] {
            let mut v = on;
            if ui.checkbox(&mut v, text).changed() {
                self.set_flag(key, v);
            }
        }
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label("Style");
            for th in &THEMES {
                if ui.radio(self.theme_name == th.name, th.name).clicked() && self.theme_name != th.name {
                    self.set_theme(th.name);
                }
            }
        });
        ui.horizontal(|ui| {
            ui.label("App");
            ui.label(RichText::new(format!("Version {VERSION}")).color(t.dim));
            if ui.add_enabled(!self.update_busy, Button::new("Update")).on_hover_text("Get the newest version of this app from GitHub. The app restarts by itself.").clicked() {
                self.update_app();
            }
        });
        ui.add_space(16.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("Mods, backups and settings are kept in {}", self.v.root.display())).color(t.dim));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("Open data folder").clicked() {
                    let _ = shell_open(&self.v.root, &[], None);
                }
            });
        });
    }

    // ---------- dialogs ----------
    fn dialog(&mut self, ui: &mut Ui, t: &'static Theme) {
        let Some(d) = self.dialogs.last_mut() else { return };
        let ctx = ui.ctx().clone();
        let mut go: Option<&'static str> = None;
        let mut add_pick = None;
        let r = egui::Modal::new(Id::unique("dialog")).show(&ctx, |ui| {
            ui.set_width(560.0);
            match d {
                Dialog::Add { q } => {
                    ui.label(RichText::new("Add mods").color(t.fg).size(20.0).strong());
                    let r = ui.add(TextEdit::singleline(q).hint_text("Search Nexus Mods and press Enter…").desired_width(f32::INFINITY));
                    if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                        go = Some("search");
                    }
                    for (i, (glyph, title, text)) in ADD_WAYS.iter().enumerate() {
                        let row = card(ui, t, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(*glyph).font(theme::icon_font(20.0)).color(t.accent));
                                ui.vertical(|ui| {
                                    ui.label(RichText::new(*title).color(t.fg).strong());
                                    dim(ui, t, text);
                                });
                            });
                            ui.min_rect()
                        });
                        if ui.interact(row, ui.scope_id().with(("add_way", i)), egui::Sense::click()).clicked() {
                            add_pick = Some(i);
                        }
                    }
                    if ui.button("Cancel").clicked() {
                        go = Some("cancel");
                    }
                }
                Dialog::SaveName { text } => {
                    ui.label(RichText::new("Save mod pack").color(t.fg).size(18.0).strong());
                    ui.label("Name of the mod pack:");
                    let r = ui.add(TextEdit::singleline(text).desired_width(f32::INFINITY));
                    let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                    ui.horizontal(|ui| {
                        if ui.add(big(t, "OK")).clicked() || enter {
                            go = Some("save");
                        }
                        if ui.button("Cancel").clicked() {
                            go = Some("cancel");
                        }
                    });
                }
                Dialog::Merge { names, picked, new } => {
                    ui.label(RichText::new("Merge mod packs").color(t.fg).size(18.0).strong());
                    dim(ui, t, "Pick the packs to merge. When two mods change the same file, the pack lower in this list wins. You can change that later in the Crash doctor tab.");
                    for (n, p) in names.iter().zip(picked.iter_mut()) {
                        let len = self.v.pack(n).map_or(0, Vec::len);
                        ui.checkbox(p, format!("{n}  ({len} mods)"));
                    }
                    ui.horizontal(|ui| {
                        ui.label("New pack");
                        ui.add(TextEdit::singleline(new).desired_width(300.0));
                    });
                    ui.horizontal(|ui| {
                        if ui.add(big(t, "▶ Merge")).clicked() {
                            go = Some("merge");
                        }
                        if ui.button("Cancel").clicked() {
                            go = Some("cancel");
                        }
                    });
                }
                Dialog::PackChoice { name, optional, old, picks: p, .. } => {
                    ui.label(RichText::new(format!("Mod pack: {name}")).color(t.fg).size(18.0).strong());
                    dim(ui, t, "Pick what to download with this mod pack.");
                    ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
                        for (title, text, files) in [
                            ("Optional mods", "The pack works without these. Tick the ones you want.", &*optional),
                            ("Mods that may not work any more", "Untick a mod to skip it.", &*old),
                        ] {
                            if files.is_empty() {
                                continue;
                            }
                            section(ui, t, title);
                            dim(ui, t, text);
                            for (id, n, why) in files {
                                if let Some(e) = p.iter_mut().find(|e| e.0 == *id) {
                                    let label = if why.is_empty() { n.clone() } else { format!("{n}  ({})", Window::pick_text(why)) };
                                    ui.checkbox(&mut e.1, label);
                                }
                            }
                        }
                    });
                    ui.horizontal(|ui| {
                        if ui.add(big(t, "Download the pack")).clicked() {
                            go = Some("choice");
                        }
                        if ui.button("Cancel").clicked() {
                            go = Some("cancel");
                        }
                    });
                }
                Dialog::Outdated { name, why, .. } => {
                    ui.label(RichText::new("This mod may not work").color(t.fg).size(18.0).strong());
                    ui.label(RichText::new(name.as_str()).color(t.accent).strong());
                    dim(ui, t, &Window::outdated_text(why));
                    ui.horizontal(|ui| {
                        if ui.add(big(t, "Download")).clicked() {
                            go = Some("get");
                        }
                        if ui.button("Skip").clicked() {
                            go = Some("skip");
                        }
                    });
                }
            }
        });
        if r.should_close() && go.is_none() && add_pick.is_none() {
            go = Some(if matches!(self.dialogs.last(), Some(Dialog::Outdated { .. })) { "skip" } else { "cancel" });
        }
        if let Some(i) = add_pick {
            return self.add_go(i);
        }
        match go {
            Some("search") => self.add_search(),
            Some("save") => self.save_pack_ok(),
            Some("merge") => self.merge_ok(),
            Some("choice") => self.choice_ok(),
            Some("get") => self.outdated_pick(true),
            Some("skip") => self.outdated_pick(false),
            Some(_) => {
                self.dialogs.pop();
            }
            None => {}
        }
    }
}
