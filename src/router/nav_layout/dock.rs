use std::rc::Rc;

use crate::{
    chat::invite::PENDING_INVITES,
    global_state::nav_bar_items,
    helpers::spawn,
    router::nav_item::{NavItemData, NavItemId},
};
use dioxus::prelude::*;

/// Minimum px of finger movement before a touch becomes a drag.
const DRAG_THRESHOLD: f64 = 10.0;

/// Cached dock-item bounds for touch hit-testing, excluding the landscape spacer.
#[derive(Clone, Copy, PartialEq)]
struct DockRect {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

/// Moves item at `from_id` to the position of `to_id`, shifting others.
///
/// Uses `peek()` first to avoid opening a write guard (and the rerender it
/// triggers on drop) when no mutation is actually needed.
fn displace_item(from_id: NavItemId, to_id: NavItemId) {
    if from_id == to_id {
        return;
    }
    let mut items = nav_bar_items().write();
    let Some(from) = items.iter().position(|i| i.id == from_id) else {
        return;
    };
    let Some(to) = items.iter().position(|i| i.id == to_id) else {
        return;
    };
    if from == to {
        return;
    }
    let item = items.remove(from);
    items.insert(to, item);
}

#[component]
fn NavItem(
    nav_item_data: NavItemData,
    dragging: Signal<NavItemData>,
    mut is_dragging: Signal<bool>,
    mut dock_rect: Signal<Option<DockRect>>,
    mut last_drag_target: Signal<Option<NavItemId>>,
    nav_el: Signal<Option<Rc<MountedData>>>,
    first_item_el: Signal<Option<Rc<MountedData>>>,
    last_item_el: Signal<Option<Rc<MountedData>>>,
    is_first: bool,
    is_last: bool,
) -> Element {
    let mut touch_start = use_signal(|| None::<(f64, f64)>);
    let mut cached_item_count = use_signal(|| 0usize);

    let end_touch = move |_: TouchEvent| {
        touch_start.set(None);
        is_dragging.set(false);
        last_drag_target.set(None);
    };

    rsx! {
        div {
            class: "relative landscape:max-h-16 flex flex-col items-center justify-center",
            draggable: true,
            onmounted: move |ev| {
                let data = ev.data();
                if is_first {
                    first_item_el.set(Some(data.clone()));
                }
                if is_last {
                    last_item_el.set(Some(data));
                }
                measure_dock(dock_rect, nav_el, first_item_el, last_item_el);
            },
            oncontextmenu: move |e| e.prevent_default(),
            ondragstart: {
                let data = nav_item_data;
                move |_| {
                    last_drag_target.set(None);
                    dragging.set(data);
                    is_dragging.set(true);
                }
            },
            ondragover: {
                let data = nav_item_data;
                move |e| {
                    let dragged_id = dragging.read().id;
                    if dragged_id != data.id && *last_drag_target.peek() != Some(data.id) {
                        displace_item(dragged_id, data.id);
                        last_drag_target.set(Some(data.id));
                    }
                    e.prevent_default();
                }
            },
            ondrop: move |_| {
                is_dragging.set(false);
                last_drag_target.set(None);
            },
            ondragend: move |_| {
                is_dragging.set(false);
                last_drag_target.set(None);
            },
            ontouchstart: {
                let data = nav_item_data;
                move |e: TouchEvent| {
                    if let Some(t) = e.data().touches().into_iter().next() {
                        touch_start.set(Some((t.client_coordinates().x, t.client_coordinates().y)));
                    }
                    dragging.set(data);
                    last_drag_target.set(None);
                    cached_item_count.set(nav_bar_items().peek().len());
                    measure_dock(dock_rect, nav_el, first_item_el, last_item_el);
                }
            },
            ontouchmove: move |e: TouchEvent| {
                let Some(touch) = e.data().touches().into_iter().next() else { return };
                let Some((sx, sy)) = *touch_start.read() else { return };
                let (x, y) = (touch.client_coordinates().x, touch.client_coordinates().y);
                if !is_dragging() {
                    if (x - sx).hypot(y - sy) < DRAG_THRESHOLD {
                        return;
                    }
                    is_dragging.set(true);
                }
                e.prevent_default();
                let item_count = *cached_item_count.peek();
                if item_count == 0 { return; }
                let Some(rect) = *dock_rect.read() else { return };
                if x < rect.x || x > rect.x + rect.w || y < rect.y || y > rect.y + rect.h { return; }
                let (coord, size) = if rect.w > rect.h {
                    (x - rect.x, rect.w)
                } else {
                    (y - rect.y, rect.h)
                };
                if size <= 0.0 {
                    return;
                }
                let target_index = (0..item_count)
                    .take_while(|&idx| {
                        coord * f64::from(u32::try_from(item_count).unwrap_or(u32::MAX))
                            >= size * f64::from(u32::try_from(idx + 1).unwrap_or(u32::MAX))
                    })
                    .count()
                    .min(item_count - 1);
                let Some(target) = nav_bar_items().peek().get(target_index).map(|item| item.id) else { return };
                if Some(target) != *last_drag_target.peek() {
                    displace_item(dragging.read().id, target);
                    last_drag_target.set(Some(target));
                }
            },
            ontouchend: end_touch,
            ontouchcancel: end_touch,

            span { class: "{nav_item_data.icon} text-base", "aria-hidden": "true" }
            if nav_item_data.id == NavItemId::Chat {
                ChatInviteBadge {}
            }
            span { class: "dock-label text-center", "{nav_item_data.label}" }

            Link {
                draggable: false,
                class: "absolute inset-0 select-none",
                to: nav_item_data.route,
                active_class: "dock-active",
                // The visible label is a sibling of this absolutely-positioned
                // link, so the `a` itself has no content to take a name from.
                // `aria-current` is supplied by `Link` itself (`href == current`.
                aria_label: "{nav_item_data.label}",
            }
        }
    }
}

/// Isolated badge component so only this subscribes to `PENDING_INVITES`.
#[component]
fn ChatInviteBadge() -> Element {
    let has_invites = use_memo(|| !PENDING_INVITES.read().is_empty());
    if !has_invites() {
        return Ok(VNode::placeholder());
    }
    rsx! { span { class: "badge badge-xs badge-accent absolute top-0 right-1" } }
}

fn measure_dock(
    mut dock_rect: Signal<Option<DockRect>>,
    nav_el: Signal<Option<Rc<MountedData>>>,
    first_item_el: Signal<Option<Rc<MountedData>>>,
    last_item_el: Signal<Option<Rc<MountedData>>>,
) {
    let Some(nav) = nav_el.peek().clone() else {
        return;
    };
    let Some(first) = first_item_el.peek().clone() else {
        return;
    };
    let Some(last) = last_item_el.peek().clone() else {
        return;
    };
    spawn(async move {
        let (Ok(nav_rect), Ok(first_rect), Ok(last_rect)) = (
            nav.get_client_rect().await,
            first.get_client_rect().await,
            last.get_client_rect().await,
        ) else {
            return;
        };
        let y = first_rect.min_y();
        let h = last_rect.max_y() - y;
        dock_rect.set(Some(DockRect {
            x: nav_rect.min_x(),
            y,
            w: nav_rect.width(),
            h,
        }));
    });
}

#[component]
pub(super) fn DockSection() -> Element {
    let dragging = use_signal(NavItemData::default);
    let is_dragging = use_signal(bool::default);
    let dock_rect: Signal<Option<DockRect>> = use_signal(|| None);
    let last_drag_target: Signal<Option<NavItemId>> = use_signal(|| None);
    let mut nav_el: Signal<Option<Rc<MountedData>>> = use_signal(|| None);
    let first_item_el: Signal<Option<Rc<MountedData>>> = use_signal(|| None);
    let last_item_el: Signal<Option<Rc<MountedData>>> = use_signal(|| None);
    let nav_bar_items = &*nav_bar_items().read();
    let item_len = nav_bar_items.len();

    rsx! {
        div {
            class: "contents",
            class: if is_dragging() { "nav-reordering" },
            nav {
                id: "po-main-dock",
                class: "dock dock-sm z-3 landscape:order-first landscape:absolute landscape:top-0 landscape:left-0 landscape:h-full landscape:w-16 landscape:flex landscape:flex-col",
                "aria-label": "Main navigation",
                onmounted: move |ev| {
                    nav_el.set(Some(ev.data()));
                    measure_dock(dock_rect, nav_el, first_item_el, last_item_el);
                },
                {nav_bar_items.iter().enumerate().map(|(idx, item)| rsx! {
                    NavItem {
                        key: "{item.label}",
                        nav_item_data: *item,
                        dragging,
                        is_dragging,
                        dock_rect,
                        last_drag_target,
                        nav_el,
                        first_item_el,
                        last_item_el,
                        is_first: idx == 0,
                        is_last: idx + 1 == item_len,
                    }
                })}
                div { class: "hidden landscape:block landscape:flex-1" }
            }
        }
    }
}
