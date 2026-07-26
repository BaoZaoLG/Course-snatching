use super::CourseApp;
use crate::app::theme::{
    number_drag_f64, number_drag_u32, quiet_button, AMBER, BLUE, CAPTION_SIZE, GLASS_SOFT, LINE,
    META_SIZE, MUTED, TEXT,
};
use crate::config::{days_in_month, ScheduleStamp};
use crate::worker::{self, LogLevel};
use eframe::egui::{self, RichText, Vec2};

impl CourseApp {
    pub(super) fn maybe_trigger_schedule(&mut self, logged: bool, running: bool, logging_in: bool) {
        // If a precise arm already started the run, mark the key fired so we don't double-trigger.
        if running {
            if let Some(key) = self.schedule_armed_for.take() {
                self.schedule_fired_for = Some(key);
            }
            return;
        }
        if !self.cfg.schedule_enabled || !logged || logging_in {
            worker::cancel_schedule_arm(&self.state);
            self.schedule_armed_for = None;
            return;
        }
        if self.cfg.cleaned_watch().is_empty() {
            worker::cancel_schedule_arm(&self.state);
            self.schedule_armed_for = None;
            return;
        }
        let Some(stamp) = ScheduleStamp::parse(&self.cfg.schedule_time) else {
            worker::cancel_schedule_arm(&self.state);
            return;
        };
        let Some(target_secs) = stamp.to_local_seconds() else {
            worker::cancel_schedule_arm(&self.state);
            return;
        };
        let key = stamp.display();
        if self.schedule_fired_for.as_deref() == Some(key.as_str()) {
            return;
        }
        let now = worker::local_now_seconds();
        // Missed the window (e.g. app opened long after the target) — mark expired, don't fire.
        if now > target_secs + 30 {
            self.schedule_fired_for = Some(key);
            worker::cancel_schedule_arm(&self.state);
            return;
        }
        if now >= target_secs {
            self.schedule_fired_for = Some(key.clone());
            self.schedule_armed_for = None;
            worker::cancel_schedule_arm(&self.state);
            self.state
                .log(LogLevel::Info, format!("定时开抢触发：{key}"));
            self.status_line = format!("定时开抢已触发（{key}）");
            self.start_grab();
            return;
        }
        // Precise background arm once per key (avoid re-arming every UI frame).
        if self.schedule_armed_for.as_deref() != Some(key.as_str()) {
            self.schedule_armed_for = Some(key.clone());
            worker::arm_schedule(self.state.clone(), self.cfg.clone(), target_secs);
            let remain = target_secs - now;
            self.status_line = format!("定时精准确待命，约 {remain}s 后开抢");
        }
    }

    pub(super) fn show_schedule_editor(&mut self, ui: &mut egui::Ui) -> bool {
        let mut dirty = false;
        let mut stamp = ScheduleStamp::parse(&self.cfg.schedule_time).unwrap_or(ScheduleStamp {
            year: 2026,
            month: 1,
            day: 1,
            hour: 8,
            minute: 0,
            second: 0,
        });

        egui::Frame::NONE
            .fill(GLASS_SOFT)
            .stroke(egui::Stroke::new(1.0, LINE))
            .corner_radius(10.0)
            .inner_margin(egui::Margin::symmetric(12, 10))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = Vec2::new(10.0, 8.0);
                    dirty |= ui
                        .checkbox(&mut self.cfg.schedule_enabled, "定时开抢")
                        .changed();
                    ui.label(
                        RichText::new("到点自动开始（需已登录且有监控目标）")
                            .size(CAPTION_SIZE)
                            .color(MUTED),
                    );
                });

                ui.add_space(8.0);
                ui.label(
                    RichText::new("开抢时刻")
                        .size(META_SIZE)
                        .strong()
                        .color(TEXT),
                );
                ui.add_space(6.0);

                // Date row: 年 月 日
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = Vec2::new(6.0, 6.0);
                    let mut year = stamp.year as u32;
                    if number_drag_u32(ui, &mut year, true, 2024..=2035, 0.2, "年", 64.0).changed()
                    {
                        stamp.year = year as i32;
                        dirty = true;
                    }
                    if number_drag_u32(ui, &mut stamp.month, true, 1..=12, 0.15, "月", 48.0)
                        .changed()
                    {
                        dirty = true;
                    }
                    let max_day = days_in_month(stamp.year, stamp.month).max(1);
                    if stamp.day > max_day {
                        stamp.day = max_day;
                        dirty = true;
                    }
                    if number_drag_u32(ui, &mut stamp.day, true, 1..=max_day, 0.15, "日", 48.0)
                        .changed()
                    {
                        dirty = true;
                    }
                });

                ui.add_space(6.0);
                // Time row: 时 分 秒
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = Vec2::new(6.0, 6.0);
                    if number_drag_u32(ui, &mut stamp.hour, true, 0..=23, 0.15, "时", 48.0)
                        .changed()
                    {
                        dirty = true;
                    }
                    if number_drag_u32(ui, &mut stamp.minute, true, 0..=59, 0.2, "分", 48.0)
                        .changed()
                    {
                        dirty = true;
                    }
                    if number_drag_u32(ui, &mut stamp.second, true, 0..=59, 0.2, "秒", 48.0)
                        .changed()
                    {
                        dirty = true;
                    }
                });

                ui.add_space(8.0);
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = Vec2::new(8.0, 6.0);
                    if ui.add(quiet_button("设为现在", 88.0)).clicked() {
                        let (y, m, d, h, mi, s) = worker::now_parts();
                        stamp = ScheduleStamp {
                            year: y,
                            month: m,
                            day: d,
                            hour: h,
                            minute: mi,
                            second: s,
                        };
                        dirty = true;
                    }
                    if ui.add(quiet_button("明天 08:00", 96.0)).clicked() {
                        let (y, m, d, _, _, _) = worker::now_parts();
                        let mut ny = y;
                        let mut nm = m;
                        let mut nd = d + 1;
                        let max = days_in_month(ny, nm);
                        if nd > max {
                            nd = 1;
                            nm += 1;
                            if nm > 12 {
                                nm = 1;
                                ny += 1;
                            }
                        }
                        stamp = ScheduleStamp {
                            year: ny,
                            month: nm,
                            day: nd,
                            hour: 8,
                            minute: 0,
                            second: 0,
                        };
                        dirty = true;
                    }
                    ui.label(
                        RichText::new(format!(
                            "设定 {}　现在 {}",
                            stamp.display(),
                            worker::now_stamp()
                        ))
                        .size(CAPTION_SIZE)
                        .color(MUTED),
                    );
                });

                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("开抢冲刺").size(META_SIZE).color(MUTED));
                    let mut burst = self.cfg.open_burst_seconds as f64;
                    if number_drag_f64(ui, &mut burst, true, 0.0..=120.0, 1.0, 0, "秒", 72.0)
                        .changed()
                    {
                        self.cfg.open_burst_seconds = burst.round().clamp(0.0, 120.0) as u32;
                        dirty = true;
                    }
                    ui.label(
                        RichText::new("开始后前 N 秒去掉轮询正抖动，首轮不等待")
                            .size(CAPTION_SIZE)
                            .color(MUTED),
                    );
                });

                if self.cfg.schedule_enabled {
                    ui.add_space(4.0);
                    if let Some(target) = stamp.to_local_seconds() {
                        let now = worker::local_now_seconds();
                        let hint = if now < target {
                            let wait = target - now;
                            let h = wait / 3600;
                            let m = (wait % 3600) / 60;
                            let s = wait % 60;
                            format!("距开抢还有 {h:02}:{m:02}:{s:02}")
                        } else if now <= target + 30 {
                            "即将触发…".into()
                        } else {
                            "该时刻已过，请重新选择".into()
                        };
                        ui.label(
                            RichText::new(hint)
                                .size(CAPTION_SIZE)
                                .color(if now < target { BLUE } else { AMBER }),
                        );
                    }
                }
            });

        if dirty {
            if let Some(valid) = stamp.validated() {
                self.cfg.schedule_time = valid.display();
                // Re-arm when user changes the target.
                self.schedule_fired_for = None;
                self.schedule_armed_for = None;
                worker::cancel_schedule_arm(&self.state);
            }
        }
        dirty
    }
}
