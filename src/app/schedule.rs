use super::CourseApp;
use crate::app::theme::{
    number_drag_f64, number_drag_u32, pal, quiet_button, CAPTION_SIZE, META_SIZE,
};
use crate::config::{days_in_month, ScheduleStamp};
use crate::worker;
use eframe::egui::{self, RichText, Vec2};

impl CourseApp {
    pub(super) fn maybe_trigger_schedule(&mut self, logged: bool, running: bool, logging_in: bool) {
        // 触发权在 worker 的精确待命任务（arm_schedule 是唯一 fire 点）：
        // UI 帧循环只按配置与共享状态决定重新待命、取消或标记过期。
        // 运行期间不动待命状态——手动开抢绝不能把定时键误标成已触发。
        if running {
            return;
        }
        if !self.cfg.schedule_enabled || !logged || logging_in {
            worker::cancel_schedule_arm(&self.state);
            return;
        }
        if self.cfg.cleaned_watch().is_empty() {
            worker::cancel_schedule_arm(&self.state);
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
        let fired = self.state.schedule_fired_matches(&key);
        let armed = self.state.schedule_armed_matches(&key);
        let now = worker::local_now_seconds();
        match worker::schedule_decision(now, target_secs, fired, armed) {
            // Missed the window (e.g. app opened long after the target) — mark expired, don't fire.
            worker::ScheduleAction::MarkExpired => {
                self.state.mark_schedule_expired(&key);
            }
            // 到点但未过宽限期时同样走 Arm：arm_schedule 的立即分支会开抢。
            worker::ScheduleAction::Arm => {
                let remain = (target_secs - now).max(0);
                worker::arm_schedule(self.state.clone(), self.cfg.clone(), key, target_secs);
                self.set_status(format!("定时精准待命，约 {remain}s 后开抢"));
            }
            worker::ScheduleAction::Noop => {}
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
            .fill(pal().glass_soft)
            .stroke(egui::Stroke::new(1.0, pal().line))
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
                            .color(pal().muted),
                    );
                });

                ui.add_space(8.0);
                ui.label(
                    RichText::new("开抢时刻")
                        .size(META_SIZE)
                        .strong()
                        .color(pal().text),
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
                        .color(pal().muted),
                    );
                });

                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("开抢冲刺").size(META_SIZE).color(pal().muted));
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
                            .color(pal().muted),
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
                                .color(if now < target {
                                    pal().blue
                                } else {
                                    pal().amber
                                }),
                        );
                    }
                }
            });

        if dirty {
            if let Some(valid) = stamp.validated() {
                let next = valid.display();
                // 只有开抢时刻真的改了才重置去重键。否则在宽限期内动一下
                // 「定时开抢」开关就会解除去重，同一时刻二次开抢。
                if next != self.cfg.schedule_time {
                    self.cfg.schedule_time = next;
                    self.state.clear_schedule_fired();
                    worker::cancel_schedule_arm(&self.state);
                }
            }
        }
        dirty
    }
}
