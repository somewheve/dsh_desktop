//! 探针：验证 LayoutJob wrap 对"纯中文无空格长段"是否真的换行。
use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, FontId};
use egui_kittest::Harness;
use std::sync::Mutex;

const CJK_LONG: &str = "无任何伪造迹象。十五篇论文全部经官方渠道验证真实存在，作者身份、期刊、时间线完全一致，代表作内容详实自洽，且与该化学家的持续研究轨迹吻合，核查深度受付费墙限制，内容层抽查两篇，如需全量全文级核查可联系作者或通过机构订阅访问，此段没有任何空格。";

static RECT: Mutex<Option<egui::Rect>> = Mutex::new(None);

#[test]
fn probe_cjk_wrap() {
    let mut harness = Harness::new_ui(|ui| {
        let mut job = LayoutJob::default();
        job.wrap.max_width = 280.0;
        job.append(
            CJK_LONG,
            0.0,
            TextFormat {
                font_id: FontId::proportional(12.0),
                color: Color32::GRAY,
                ..Default::default()
            },
        );
        let galley = ui.fonts_mut(|f| f.layout_job(job));
        let (rect, _resp) = ui.allocate_exact_size(galley.size(), egui::Sense::hover());
        ui.painter().galley(rect.min, galley, Color32::WHITE);
        *RECT.lock().unwrap() = Some(rect);
    });
    harness.set_size(egui::Vec2::new(300.0, 400.0));
    harness.run_steps(3);
    let rect = *RECT.lock().unwrap();
    println!("cjk galley rect: {rect:?}");
    assert!(
        rect.is_some_and(|r| r.height() > 30.0),
        "纯中文未换行: {rect:?}"
    );
}
