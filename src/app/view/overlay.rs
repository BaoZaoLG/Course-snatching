//! 浮层：吐司与各类确认框。
//!
//! A-02：视图函数原先全部挤在 app/mod.rs 里（show_header 一个就 255 行、
//! 10 个参数，show_watch_panel 闭包嵌套六层）。拆开后每个文件对应界面上的
//! 一块区域；状态与动作仍留在 app/mod.rs，这里只负责画。

use super::super::*;

impl CourseApp {
    pub(in crate::app) fn show_overlays(&mut self, root_ui: &mut egui::Ui) {
        if let Some((_, text, ok)) = self.toast.clone() {
            egui::Area::new(egui::Id::new("toast"))
                .anchor(Align2::CENTER_TOP, [0.0, 18.0])
                .order(egui::Order::Foreground)
                .show(root_ui.ctx(), |ui| {
                    egui::Frame::NONE
                        .fill(if ok {
                            pal().success_fill
                        } else {
                            pal().danger_fill
                        })
                        .stroke(egui::Stroke::new(
                            1.0,
                            if ok { pal().green } else { pal().red },
                        ))
                        .corner_radius(10.0)
                        .inner_margin(egui::Margin::symmetric(14, 10))
                        .show(ui, |ui| {
                            ui.set_max_width(560.0);
                            ui.label(RichText::new(text).size(META_SIZE).color(if ok {
                                pal().green
                            } else {
                                pal().red
                            }));
                        });
                });
        }
        if self.show_first_run {
            egui::Window::new("欢迎使用选课助手")
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
                .show(root_ui.ctx(), |ui| {
                    ui.set_min_width(420.0);
                    ui.label("三步开始：");
                    ui.label("1. 登录账号");
                    ui.label("2. 刷新课程");
                    ui.label("3. 点右侧「监控」加入目标后开始抢课");
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new("请仅用于本人账号，合理控制请求频率，遵守学校规定。")
                            .size(CAPTION_SIZE)
                            .color(pal().muted),
                    );
                    ui.add_space(10.0);
                    if ui
                        .add(primary_button("知道了", pal().blue, 100.0))
                        .clicked()
                    {
                        self.show_first_run = false;
                        self.cfg.first_run_ack = true;
                        self.save_config();
                    }
                });
        }
        // 五处几乎逐字重复的确认框现在共用 theme::confirm_dialog：
        // 「危险操作必须二次确认」这条规则有了唯一的实现。
        if self.confirm_logout {
            match confirm_dialog(
                root_ui.ctx(),
                "确认退出登录",
                |ui| {
                    ui.label("退出后需要重新登录才能继续。");
                },
                "退出",
                true,
            ) {
                Some(true) => {
                    self.confirm_logout = false;
                    worker::logout(&self.state);
                    self.set_status("已退出登录");
                }
                Some(false) => self.confirm_logout = false,
                None => {}
            }
        }
        if self.confirm_clear_logs {
            match confirm_dialog(
                root_ui.ctx(),
                "确认清空日志",
                |ui| {
                    ui.label("清空后无法恢复。");
                },
                "清空",
                true,
            ) {
                Some(true) => {
                    self.confirm_clear_logs = false;
                    self.state.logs.lock().clear();
                }
                Some(false) => self.confirm_clear_logs = false,
                None => {}
            }
        }
        // F-05：退课不可逆——名额立刻放给别人，且未必抢得回来。
        if let Some(lesson_id) = self.confirm_drop.clone() {
            let course = self
                .state
                .elected
                .lock()
                .iter()
                .find(|lesson| lesson.id == lesson_id)
                .map(|lesson| format!("{} · {}", lesson.no, lesson.name))
                .unwrap_or_else(|| lesson_id.clone());
            egui::Window::new("确认退课")
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
                .show(root_ui.ctx(), |ui| {
                    ui.set_min_width(400.0);
                    ui.label(
                        RichText::new(format!("即将退掉：{course}"))
                            .size(BODY_SIZE)
                            .color(pal().text),
                    );
                    ui.label(
                        RichText::new(
                            "退课不可逆：名额会立刻放给别人，未必还能抢回来。请确认这不是你要保留的课。",
                        )
                        .size(CAPTION_SIZE)
                        .color(pal().red),
                    );
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.add(quiet_button("取消", 72.0)).clicked() {
                            self.confirm_drop = None;
                        }
                        if ui.add(soft_danger_button("确认退课", 96.0)).clicked() {
                            self.confirm_drop = None;
                            worker::drop_lesson(
                                self.state.clone(),
                                self.cfg.profile_id.clone(),
                                lesson_id.clone(),
                            );
                        }
                    });
                });
        }
        // F-01：教务在连续登录失败若干次后强制上验证码。此前没有任何获取或
        // 提交验证码的路径，一旦触发工具就永久卡死——而 login() 最多重试 4 次，
        // 恰好容易把学校推到这个阈值上。不做 OCR，一次性手填即可。
        let captcha = self.state.pending_captcha.lock().clone();
        if let Some(bytes) = captcha {
            egui::Window::new("需要输入验证码")
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
                .show(root_ui.ctx(), |ui| {
                    ui.set_min_width(320.0);
                    ui.label(
                        RichText::new("教务系统要求验证码（通常是连续登录失败后触发）。")
                            .size(META_SIZE)
                            .color(pal().text),
                    );
                    ui.add_space(8.0);
                    // 按字节长度做缓存键：换一张图长度几乎必然不同，
                    // 相同则没必要重新解码。
                    let key = bytes.len();
                    if self.captcha_texture.as_ref().map(|(k, _)| *k) != Some(key) {
                        if let Ok(image) = image_from_bytes(&bytes) {
                            let texture = ui.ctx().load_texture(
                                "captcha",
                                image,
                                egui::TextureOptions::LINEAR,
                            );
                            self.captcha_texture = Some((key, texture));
                        } else {
                            self.captcha_texture = None;
                        }
                    }
                    match &self.captcha_texture {
                        Some((_, texture)) => {
                            ui.add(
                                egui::Image::new(texture)
                                    .fit_to_original_size(1.0)
                                    .max_height(64.0),
                            );
                        }
                        None => {
                            ui.label(
                                RichText::new(
                                    "验证码图片无法显示（格式不受支持）。\
                                     可在浏览器中登录一次教务系统解除限制后重试。",
                                )
                                .size(CAPTION_SIZE)
                                .color(pal().amber),
                            );
                        }
                    }
                    ui.add_space(8.0);
                    let entry = ui.add(
                        egui::TextEdit::singleline(&mut self.captcha_text)
                            .hint_text("输入图中字符")
                            .desired_width(160.0),
                    );
                    let submit_by_enter =
                        entry.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.add(quiet_button("取消", 72.0)).clicked() {
                            *self.state.pending_captcha.lock() = None;
                            self.captcha_text.clear();
                            self.captcha_texture = None;
                            self.state.set_message("已取消登录");
                        }
                        let can_submit = !self.captcha_text.trim().is_empty();
                        if (ui
                            .add_enabled(can_submit, primary_button("提交", pal().blue, 88.0))
                            .clicked()
                            || (submit_by_enter && can_submit))
                            && can_submit
                        {
                            self.captcha_input = Some(self.captcha_text.trim().to_string());
                            self.captcha_text.clear();
                            self.captcha_texture = None;
                            *self.state.pending_captcha.lock() = None;
                            self.do_login();
                        }
                    });
                });
        }
        if self.confirm_export_raw_diagnostics {
            egui::Window::new("确认导出原始页面")
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
                .show(root_ui.ctx(), |ui| {
                    ui.set_min_width(430.0);
                    ui.label("原始调试页面会先脱敏（抹掉常见的 Cookie/密码/令牌字段，含选课提交表单的页面整份排除），但脱敏不保证覆盖全部，页面仍可能含姓名、学号与你的课表，仅应发送给你信任的支持人员。");
                    ui.label(
                        RichText::new("默认诊断包不会包含这些内容。")
                            .size(CAPTION_SIZE)
                            .color(pal().muted),
                    );
                    ui.horizontal(|ui| {
                        if ui.add(quiet_button("取消", 72.0)).clicked() {
                            self.confirm_export_raw_diagnostics = false;
                        }
                        if ui.add(soft_danger_button("仍要包含", 90.0)).clicked() {
                            self.confirm_export_raw_diagnostics = false;
                            self.export_diagnostics(true);
                        }
                    });
                });
        }
        // A-02：按序号而不是下标确认。
        //
        // 这个对话框是非模态的：它开着的时候用户仍然可以点 ↑↓ 调整优先级或
        // 「清理失败」。存下标的话，列表一变，下标就指向了另一门课——点确认
        // 会静默删掉一门用户根本没打算删的课，而且不会有任何报错。
        if let Some(serial) = self.confirm_remove.clone() {
            let still_present = self.cfg.watch_serials.iter().any(|item| item == &serial);
            egui::Window::new("确认移除监控")
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
                .show(root_ui.ctx(), |ui| {
                    if !still_present {
                        // 目标在对话框开着时已经被别的操作移除了。
                        ui.label(
                            RichText::new(format!("{serial} 已不在监控列表中。"))
                                .size(META_SIZE)
                                .color(pal().muted),
                        );
                        if ui.add(quiet_button("知道了", 88.0)).clicked() {
                            self.confirm_remove = None;
                        }
                        return;
                    }
                    ui.label(format!("移除监控目标：{serial}？"));
                    ui.horizontal(|ui| {
                        if ui.add(quiet_button("取消", 72.0)).clicked() {
                            self.confirm_remove = None;
                        }
                        if ui.add(soft_danger_button("移除", 72.0)).clicked() {
                            self.cfg.watch_serials.retain(|item| item != &serial);
                            self.cfg.watch_lesson_ids.remove(&serial);
                            self.cfg.watch_meta.remove(&serial);
                            self.cfg.watch_groups.remove(&serial);
                            self.save_config();
                            self.confirm_remove = None;
                        }
                    });
                });
        }

        if self.confirm_start_grab {
            egui::Window::new("确认开始抢课")
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
                .show(root_ui.ctx(), |ui| {
                    ui.set_min_width(460.0);
                    let targets = self.cfg.cleaned_watch();
                    ui.label(
                        RichText::new(format!("监控目标 {} 门", targets.len()))
                            .strong()
                            .size(BODY_SIZE)
                            .color(pal().text),
                    );
                    ui.add_space(6.0);
                    egui::ScrollArea::vertical()
                        .max_height(140.0)
                        .show(ui, |ui| {
                            for serial in &targets {
                                let name = self
                                    .cfg
                                    .watch_meta
                                    .get(serial)
                                    .map(|m| m.name.as_str())
                                    .filter(|s| !s.is_empty())
                                    .unwrap_or("未命名课程");
                                ui.label(
                                    RichText::new(format!("· {serial}  {name}"))
                                        .size(META_SIZE)
                                        .color(pal().text),
                                );
                            }
                        });
                    ui.add_space(10.0);
                    ui.separator();
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new(format!(
                            "间隔 {:.2} 秒　网络异常时自动降速 {}　优先余量 {}　仅监控 {}",
                            self.cfg.interval_seconds,
                            on_off(self.cfg.adaptive_interval),
                            on_off(self.cfg.grab_seats_first),
                            on_off(self.cfg.monitor_only),
                        ))
                        .size(META_SIZE)
                        .color(pal().muted),
                    );
                    ui.label(
                        RichText::new(format!(
                            "结果通知 {}　提示音 {}",
                            on_off(self.cfg.notify_enabled),
                            on_off(self.cfg.sound_enabled),
                        ))
                        .size(META_SIZE)
                        .color(pal().muted),
                    );
                    ui.add_space(8.0);
                    ui.label(
                        RichText::new("开抢自检")
                            .strong()
                            .size(META_SIZE)
                            .color(pal().text),
                    );
                    for (ok, line) in self.preflight_checks() {
                        ui.label(
                            RichText::new(format!("{} {}", if ok { "✓" } else { "!" }, line))
                                .size(META_SIZE)
                                .color(if ok { pal().green } else { pal().amber }),
                        );
                    }
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        if ui.add(quiet_button("取消", 84.0)).clicked() {
                            self.confirm_start_grab = false;
                        }
                        let can_start = self.preflight_checks().iter().all(|(ok, _)| *ok);
                        if ui
                            .add_enabled(can_start, primary_button("确认开始", pal().green, 110.0))
                            .clicked()
                        {
                            self.confirm_start_grab = false;
                            self.start_grab();
                        }
                    });
                });
        }

        if let Some(summary) = self.result_summary.clone() {
            egui::Window::new("本轮结果摘要")
                .collapsible(false)
                .resizable(false)
                .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
                .show(root_ui.ctx(), |ui| {
                    ui.set_min_width(420.0);
                    ui.label(RichText::new(&summary).size(META_SIZE).color(pal().text));
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        if ui
                            .add(primary_button("知道了", pal().blue, 100.0))
                            .clicked()
                        {
                            self.result_summary = None;
                        }
                        if ui.add(quiet_button("导出摘要", 100.0)).clicked() {
                            self.export_result_summary(&summary);
                        }
                    });
                });
        }
    }
}
