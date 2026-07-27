//! 课程表：虚拟化列表 + 手绘行。
//!
//! A-02：视图函数原先全部挤在 app/mod.rs 里（show_header 一个就 255 行、
//! 10 个参数，show_watch_panel 闭包嵌套六层）。拆开后每个文件对应界面上的
//! 一块区域；状态与动作仍留在 app/mod.rs，这里只负责画。

use super::super::*;

impl CourseApp {
    pub(in crate::app) fn show_course_catalog(
        &mut self,
        root_ui: &mut egui::Ui,
        running: bool,
        lesson_count: usize,
    ) {
        egui::CentralPanel::default()
            .frame(
                egui::Frame::NONE
                    .fill(pal().fog)
                    .inner_margin(egui::Margin {
                        left: 6,
                        right: 12,
                        top: 10,
                        bottom: 12,
                    }),
            )
            .show(root_ui, |ui| {
                glass_surface(ui, true, |ui| {
                    ui.horizontal(|ui| {
                        ui.set_min_height(CONTROL_H);
                        ui.spacing_mut().item_spacing.x = 10.0;
                        ui.label(
                            RichText::new("可选课程")
                                .strong()
                                .size(PANEL_TITLE)
                                .color(pal().text),
                        );
                        ui.add_sized(
                            [240.0, CONTROL_H],
                            egui::TextEdit::singleline(&mut self.filter)
                                // 固定 Id：Ctrl+F 要能把焦点移到这里。
                                .id(SEARCH_FIELD_ID.with("catalog_search"))
                                .hint_text("搜索 序号 / 名称 / 教师")
                                .vertical_align(Align::Center),
                        );
                        if ui.checkbox(&mut self.only_available, "仅有余量").changed() {
                            self.cfg.only_available = self.only_available;
                            self.save_config();
                        }
                        egui::ComboBox::from_id_salt("catalog_sort")
                            .selected_text(match self.catalog_sort {
                                CatalogSort::Default => "默认排序",
                                CatalogSort::SeatsFirst => "余量优先",
                                CatalogSort::Name => "按名称",
                            })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut self.catalog_sort,
                                    CatalogSort::Default,
                                    "默认排序",
                                );
                                ui.selectable_value(
                                    &mut self.catalog_sort,
                                    CatalogSort::SeatsFirst,
                                    "余量优先",
                                );
                                ui.selectable_value(
                                    &mut self.catalog_sort,
                                    CatalogSort::Name,
                                    "按名称",
                                );
                            });
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            ui.label(
                                RichText::new(format!("共 {lesson_count}"))
                                    .size(META_SIZE)
                                    .color(pal().muted),
                            );
                        });
                    });
                    ui.add_space(12.0);

                    // 派生视图缓存：只有课表本身（revision）或筛选/排序条件
                    // 变了才重算。running 时重绘间隔曾是 50ms，即每秒约 20 次
                    // 把整张课表深拷贝、逐条 to_lowercase 再排序——既是 CPU 与
                    // allocator 压力，也让 UI 线程和抢课网络线程争抢同一把锁，
                    // 而 worker runtime 只有 2 个线程。
                    self.refresh_catalog_view();
                    let filtered = self.catalog_view.rows.clone();
                    // persist search text lightly
                    if self.cfg.filter != self.filter {
                        self.cfg.filter = self.filter.clone();
                        self.save_config();
                    }
                    let shown = filtered.len();
                    let watched: HashSet<String> = self.cfg.watch_serials.iter().cloned().collect();
                    let watched_lesson_ids = self.cfg.watch_lesson_ids.clone();

                    let full_w = ui.available_width().max(1.0);
                    let action_w = 76.0;
                    let capacity_w = (full_w * 0.12).clamp(88.0, 116.0);
                    let teacher_w = (full_w * 0.16).clamp(110.0, 150.0);
                    let serial_w = (full_w * 0.22).clamp(168.0, 236.0);
                    let name_w = (full_w - serial_w - teacher_w - capacity_w - action_w).max(180.0);
                    let widths = [serial_w, name_w, teacher_w, capacity_w, action_w];
                    draw_table_header(ui, full_w, &widths);
                    ui.add_space(2.0);

                    if filtered.is_empty() {
                        egui::ScrollArea::vertical()
                            .id_salt("catalog")
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                ui.set_min_width(full_w);
                                ui.add_space(54.0);
                                empty_hint(
                                    ui,
                                    if lesson_count == 0 {
                                        "尚未载入课程"
                                    } else {
                                        "没有符合条件的课程"
                                    },
                                    if lesson_count == 0 {
                                        "登录后点击“刷新课程”即可载入"
                                    } else {
                                        "请调整搜索内容或筛选条件"
                                    },
                                );
                            });
                    } else {
                        // 只画可视区。可视区通常只有约 15 行，此前却要为最多
                        // 800 行分配 rect、跑 animate_bool_with_time、生成 galley
                        // 并 tessellate——这是 running 时最大的单帧开销。行高
                        // 固定，天然适配 show_rows；顺带去掉 800 行的硬截断，
                        // 课程再多也不会被悄悄截掉。
                        egui::ScrollArea::vertical()
                            .id_salt("catalog")
                            .auto_shrink([false, false])
                            .show_rows(ui, CATALOG_ROW_H, filtered.len(), |ui, range| {
                                ui.set_min_width(full_w);
                                for index in range {
                                    let lesson = &filtered[index];
                                    let serial_watched = watched.contains(&lesson.no);
                                    let selected_id = watched_lesson_ids.get(&lesson.no);
                                    let already = selected_id.is_some_and(|id| id == &lesson.id);
                                    let switching = serial_watched
                                        && selected_id.is_some_and(|id| id != &lesson.id);
                                    let needs_specifying = serial_watched && selected_id.is_none();
                                    let row_h = CATALOG_ROW_H;
                                    let (row_rect, row_response) = ui.allocate_exact_size(
                                        Vec2::new(full_w, row_h),
                                        Sense::click(),
                                    );
                                    let base = if index % 2 == 0 {
                                        Color32::TRANSPARENT
                                    } else {
                                        pal().row_alt
                                    };
                                    // 纯 painter 绘制不产生任何 widget 语义：
                                    // Cargo.toml 开了 accesskit，但读屏软件看到
                                    // 的主内容区是一片空白。至少让每一行可枚举、
                                    // 能被读出来。
                                    row_response.widget_info(|| {
                                        egui::WidgetInfo::labeled(
                                            egui::WidgetType::Button,
                                            true,
                                            format!(
                                                "{} {} {} 已选 {}{}",
                                                lesson.no,
                                                lesson.name,
                                                lesson.teachers,
                                                lesson.capacity_text(),
                                                if already { "（监控中）" } else { "" }
                                            ),
                                        )
                                    });
                                    let hover = ui.ctx().animate_bool_with_time(
                                        ui.id().with(("course_row", &lesson.id)),
                                        row_response.hovered(),
                                        0.12,
                                    );
                                    ui.painter().rect_filled(
                                        row_rect,
                                        0.0,
                                        mix_color(base, pal().row_hover, hover),
                                    );
                                    ui.painter().hline(
                                        row_rect.x_range(),
                                        row_rect.bottom(),
                                        egui::Stroke::new(1.0, pal().row_line),
                                    );

                                    let mut x = row_rect.left();
                                    let mut columns = [egui::Rect::ZERO; 5];
                                    for column in 0..5 {
                                        columns[column] = egui::Rect::from_min_size(
                                            egui::pos2(x, row_rect.top()),
                                            Vec2::new(widths[column], row_h),
                                        );
                                        x += widths[column];
                                    }
                                    ui.painter().text(
                                        columns[0].left_center() + Vec2::new(12.0, 0.0),
                                        Align2::LEFT_CENTER,
                                        &lesson.no,
                                        egui::FontId::monospace(BODY_SIZE),
                                        pal().text,
                                    );
                                    let name_show = truncate_ui_text(&lesson.name, 22);
                                    ui.painter().text(
                                        columns[1].left_center() + Vec2::new(8.0, 0.0),
                                        Align2::LEFT_CENTER,
                                        name_show,
                                        egui::FontId::proportional(BODY_SIZE),
                                        pal().text,
                                    );
                                    ui.painter().text(
                                        columns[2].left_center() + Vec2::new(8.0, 0.0),
                                        Align2::LEFT_CENTER,
                                        &lesson.teachers,
                                        egui::FontId::proportional(META_SIZE),
                                        pal().muted,
                                    );
                                    let capacity_color = if lesson.has_seat() {
                                        pal().green
                                    } else if !lesson.capacity_known() {
                                        pal().muted
                                    } else {
                                        pal().red
                                    };
                                    ui.painter().text(
                                        columns[3].left_center() + Vec2::new(8.0, 0.0),
                                        Align2::LEFT_CENTER,
                                        lesson.capacity_text(),
                                        egui::FontId::proportional(META_SIZE),
                                        capacity_color,
                                    );

                                    let button_text = if already {
                                        "已加入"
                                    } else if switching {
                                        "切换"
                                    } else if needs_specifying {
                                        "指定"
                                    } else {
                                        "监控"
                                    };
                                    let button_rect = egui::Rect::from_center_size(
                                        columns[4].center(),
                                        Vec2::new(58.0, 28.0),
                                    );
                                    let button_response = ui.interact(
                                        button_rect,
                                        ui.id().with(("monitor", index, &lesson.id)),
                                        Sense::click(),
                                    );
                                    let button_hover = ui.ctx().animate_bool_with_time(
                                        ui.id().with(("monitor_hover", &lesson.id)),
                                        button_response.hovered(),
                                        0.12,
                                    );
                                    let fill = if already {
                                        pal().disabled_fill
                                    } else {
                                        mix_color(pal().quiet_fill, pal().quiet_hover, button_hover)
                                    };
                                    ui.painter().rect_filled(button_rect, 8.0, fill);
                                    ui.painter().rect_stroke(
                                        button_rect,
                                        8.0,
                                        egui::Stroke::new(1.0, pal().line),
                                        egui::StrokeKind::Inside,
                                    );
                                    ui.painter().text(
                                        button_rect.center(),
                                        Align2::CENTER_CENTER,
                                        button_text,
                                        egui::FontId::proportional(META_SIZE),
                                        if already { pal().muted } else { pal().blue },
                                    );
                                    if !running
                                        && !already
                                        && (button_response.clicked()
                                            || row_response.double_clicked())
                                    {
                                        self.add_watch_lesson(lesson);
                                    }
                                }
                                if shown > 800 {
                                    ui.add_space(8.0);
                                    ui.label(
                                        RichText::new(format!(
                                            "当前筛选 {shown} 条，仅显示前 800 条"
                                        ))
                                        .size(12.0)
                                        .color(pal().muted),
                                    );
                                }
                            });
                    }
                });
            });
    }
}
