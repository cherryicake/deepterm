use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use gpui::prelude::*;
use gpui::{
    App, ClipboardItem, Context, DismissEvent, Entity, FocusHandle, Focusable, IntoElement,
    MouseButton, Pixels, Point, Render, ScrollHandle, SharedString, Subscription, Window, anchored,
    deferred, div, point, px,
};

use crate::assets::Icon;
use crate::components::callbacks::OptionalStrCallback;
use crate::components::input::{EditorInput, InputEvent, InputState};
use crate::components::menu::{ContextMenuExt, PopupMenu, PopupMenuItem};
use crate::components::modal::{Modal, ModalState};
use crate::sidebar_view::folder_dialog::{FolderDialogMode, FolderNameDialog};
use crate::sidebar_view::host_form::NewHostForm;
use crate::sidebar_view::hosts_view::{
    ConnectHost, ConnectHostViaProxy, CopyHostAddress, DeleteHost, EditHost, Host, folder_subtitle,
    load_grouped_hosts, load_grouped_hosts_from, sort_hosts,
};
use crate::sidebar_view::style::{hover_bg, round_icon_button, row_selection_state};
use crate::ssh::config::{
    HostGroup, SortMode, SshConnectionConfig, SshGlobalConfig, sort_host_groups,
};
use crate::ssh::keychain::KeychainStore;
use crate::theme::ActiveTheme;

/// SSH 主机回调（连接 / 编辑主机时触发），参数为该主机的连接配置。
type HostConfigCallback = Rc<dyn Fn(&SshConnectionConfig, &mut Window, &mut App)>;

/// 当前选中的主机配置，按列表顺序返回。
///
/// `via_proxy` 决定是否给这批配置打上「经系统代理」标记（见
/// [`SshConnectionConfig::use_system_proxy`]）。
///
/// 返回的是**克隆**：`configs` 是本帧列表的数据源，就地置位会让同一帧里其它
/// 读取方（列表渲染、编辑弹窗）看到被污染的配置，也会让「用代理连过一次」黏在
/// 内存里的那份配置上。
fn selected_configs(
    configs: &[SshConnectionConfig],
    selected_ids: &HashSet<SharedString>,
    via_proxy: bool,
) -> Vec<SshConnectionConfig> {
    configs
        .iter()
        .filter(|config| selected_ids.iter().any(|id| id.as_ref() == config.id()))
        .map(|config| {
            let mut picked = config.clone();
            picked.use_system_proxy = via_proxy;
            picked
        })
        .collect()
}

/// 主机右键菜单顶部的两个连接项（直连 / 经系统代理）。
///
/// 两项必须各挂各的动作：接反了界面上完全看不出来，只有连上海外主机才会发现
/// 代理没生效（或本机连接被误送进代理）。
fn connect_menu_items(
    menu: PopupMenu,
    count: usize,
    icon: fn(&'static str) -> crate::components::button::Icon,
) -> PopupMenu {
    let conn_label = if count == 1 {
        "连接".to_string()
    } else {
        format!("连接 {} 台主机", count)
    };
    let proxy_label = if count == 1 {
        "使用系统代理连接".to_string()
    } else {
        format!("使用系统代理连接 {} 台主机", count)
    };
    menu.menu_with_icon(&conn_label, icon("icons/link.svg"), Box::new(ConnectHost))
        .menu_with_icon(
            proxy_label,
            icon("icons/network.svg"),
            Box::new(ConnectHostViaProxy),
        )
        .separator()
}

/// Host 侧边栏视图组件，管理 Host 列表和 New Host Modal。文件夹=列表行，两级导航，无折叠。
pub struct HostView {
    focus_handle: FocusHandle,
    scroll_handle: ScrollHandle,
    selected_ids: HashSet<SharedString>,
    host_form: Option<Entity<NewHostForm>>,
    folder_dialog: Option<Entity<FolderNameDialog>>,
    modal_state: Entity<ModalState>,
    active_modal: ActiveModal,
    on_select: OptionalStrCallback,
    on_connect: Option<HostConfigCallback>,
    on_edit_host: Option<HostConfigCallback>,
    context_menu_host_id: Option<SharedString>,
    context_menu_folder_id: Option<SharedString>,
    search_input: Entity<InputState>,
    _search_subscription: Subscription,
    search_query: SharedString,
    current_folder: Option<SharedString>,
    header_menu: Option<Entity<PopupMenu>>,
    header_menu_position: Option<Point<Pixels>>,
    header_menu_subscription: Option<Subscription>,
    /// 渲染缓存：配置/钥匙串的 `Arc` 身份变化或排序设置变化时才重建。
    grouped_cache: Option<GroupedHostsCache>,
}

/// 主机列表的派生数据缓存。持有配置/钥匙串的 `Arc`，用于 `ptr_eq` 判断
/// 底层文件是否变化；避免每帧 `load_grouped_hosts` + 深拷贝。
struct GroupedHostsCache {
    config: Arc<SshGlobalConfig>,
    keychain: Arc<KeychainStore>,
    hosts: Rc<Vec<Host>>,
    configs: Rc<Vec<SshConnectionConfig>>,
    groups: Rc<Vec<HostGroup>>,
    host_counts: HashMap<String, usize>,
    folder_sort: SortMode,
    host_sort: SortMode,
}

#[derive(Clone, Copy, PartialEq)]
enum ActiveModal {
    None,
    Host,
    Folder,
}

impl HostView {
    pub fn new(window: &mut Window, cx: &mut App) -> Entity<Self> {
        cx.new(|cx| {
            let focus_handle = cx.focus_handle();
            let scroll_handle = ScrollHandle::new();

            let search_input = cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("搜索主机 / 文件夹")
                    .placeholder_font_size(px(13.0))
                    .font_size(px(13.0))
            });

            let subscription = cx.subscribe(
                &search_input,
                |this: &mut HostView, _, event: &InputEvent, cx| {
                    if let InputEvent::Change = event {
                        let text = this.search_input.read(cx).value().to_string();
                        this.search_query = text.to_lowercase().into();
                        this.prune_selection_to_visible_hosts();
                        cx.notify();
                    }
                },
            );

            Self {
                focus_handle,
                scroll_handle,
                selected_ids: HashSet::new(),
                host_form: None,
                folder_dialog: None,
                modal_state: ModalState::entity(cx),
                active_modal: ActiveModal::None,
                on_select: None,
                on_connect: None,
                on_edit_host: None,
                context_menu_host_id: None,
                context_menu_folder_id: None,
                search_input,
                _search_subscription: subscription,
                search_query: SharedString::new_static(""),
                current_folder: None,
                header_menu: None,
                header_menu_position: None,
                header_menu_subscription: None,
                grouped_cache: None,
            }
        })
    }

    pub fn on_select<F>(&mut self, callback: F)
    where
        F: Fn(&str, &mut Window, &mut App) + 'static,
    {
        self.on_select = Some(Rc::new(callback));
    }

    pub fn on_connect<F>(&mut self, callback: F)
    where
        F: Fn(&SshConnectionConfig, &mut Window, &mut App) + 'static,
    {
        self.on_connect = Some(Rc::new(callback));
    }

    pub fn on_edit_host<F>(&mut self, callback: F)
    where
        F: Fn(&SshConnectionConfig, &mut Window, &mut App) + 'static,
    {
        self.on_edit_host = Some(Rc::new(callback));
    }

    fn host_matches_query(host: &Host, query: &str) -> bool {
        if query.is_empty() {
            return true;
        }
        host.name.to_lowercase().contains(query)
            || host
                .hostname
                .as_ref()
                .is_some_and(|h| h.to_lowercase().contains(query))
            || host
                .username
                .as_ref()
                .is_some_and(|u| u.to_lowercase().contains(query))
            || host
                .auth_label
                .as_ref()
                .is_some_and(|a| a.to_lowercase().contains(query))
    }

    fn folder_matches_query(folder: &HostGroup, query: &str) -> bool {
        if query.is_empty() {
            return true;
        }
        folder.name.to_lowercase().contains(query)
    }

    fn prune_selection_to_visible_hosts(&mut self) {
        let query = self.search_query.as_ref();
        let (hosts, _, _) = load_grouped_hosts();
        let current_folder = self.current_folder.as_deref();
        let visible: HashSet<SharedString> = hosts
            .iter()
            .filter(|host| Self::host_matches_query(host, query))
            .filter(|host| match current_folder {
                Some(fid) => host.group_id.as_deref() == Some(fid),
                None => host.group_id.is_none(),
            })
            .map(|host| host.id.clone())
            .collect();
        self.selected_ids.retain(|id| visible.contains(id));
    }

    fn close_modal(&mut self, cx: &mut App) {
        self.modal_state.update(cx, |modal, cx| modal.close(cx));
        self.active_modal = ActiveModal::None;
    }

    fn open_host_modal(
        &mut self,
        target_group: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let form = self
            .host_form
            .get_or_insert_with(|| NewHostForm::new(window, cx))
            .clone();
        let host_view = cx.entity().clone();
        form.update(cx, |form, cx| {
            form.reset(cx);
            if let Some(gid) = target_group {
                form.set_target_group(Some(gid), cx);
            }
            let close = {
                let host_view = host_view.clone();
                move |cx: &mut App| host_view.update(cx, |this, cx| this.close_modal(cx))
            };
            form.on_cancel({
                let close = close.clone();
                move |cx| close(cx)
            });
            form.on_saved(move |cx| close(cx));
        });
        self.active_modal = ActiveModal::Host;
        self.modal_state.update(cx, |modal, cx| modal.open(cx));
    }

    fn open_edit_modal(
        &mut self,
        config: &SshConnectionConfig,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let form = self
            .host_form
            .get_or_insert_with(|| NewHostForm::new(window, cx))
            .clone();
        let host_view = cx.entity().clone();
        form.update(cx, |form, cx| {
            form.edit_config(config, cx);
            let close = {
                let host_view = host_view.clone();
                move |cx: &mut App| host_view.update(cx, |this, cx| this.close_modal(cx))
            };
            form.on_cancel({
                let close = close.clone();
                move |cx| close(cx)
            });
            form.on_saved(move |cx| close(cx));
        });
        self.active_modal = ActiveModal::Host;
        self.modal_state.update(cx, |modal, cx| modal.open(cx));
    }

    fn open_create_folder_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dialog = FolderNameDialog::new(window, cx, FolderDialogMode::Create);
        let host_view = cx.entity().clone();
        let dialog_weak = dialog.downgrade();
        dialog.update(cx, |dialog, _| {
            dialog.on_cancel({
                let host_view = host_view.clone();
                move |cx| host_view.update(cx, |this, cx| this.close_modal(cx))
            });
            dialog.on_confirm({
                let host_view = host_view.clone();
                move |name, cx| {
                    let mut cfg = SshGlobalConfig::load();
                    match cfg.create_group(&name) {
                        Ok(_) => {
                            let _ = cfg.save();
                            host_view.update(cx, |this, cx| {
                                this.close_modal(cx);
                                cx.notify();
                            });
                        }
                        Err(err) => {
                            if let Some(d) = dialog_weak.upgrade() {
                                d.update(cx, |dlg, cx| dlg.set_error(err, cx));
                            }
                        }
                    }
                }
            });
        });
        self.folder_dialog = Some(dialog);
        self.active_modal = ActiveModal::Folder;
        self.modal_state.update(cx, |modal, cx| modal.open(cx));
    }

    fn open_rename_folder_dialog(
        &mut self,
        folder_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(group) = SshGlobalConfig::load().find_group(&folder_id).cloned() else {
            return;
        };
        let dialog = FolderNameDialog::new(
            window,
            cx,
            FolderDialogMode::Rename {
                old_name: group.name.clone(),
            },
        );
        let host_view = cx.entity().clone();
        let dialog_weak = dialog.downgrade();
        dialog.update(cx, |dialog, _| {
            dialog.on_cancel({
                let host_view = host_view.clone();
                move |cx| host_view.update(cx, |this, cx| this.close_modal(cx))
            });
            let fid = folder_id.clone();
            dialog.on_confirm({
                let host_view = host_view.clone();
                move |new_name, cx| {
                    let mut cfg = SshGlobalConfig::load();
                    match cfg.rename_group(&fid, &new_name) {
                        Ok(_) => {
                            let _ = cfg.save();
                            host_view.update(cx, |this, cx| {
                                this.close_modal(cx);
                                cx.notify();
                            });
                        }
                        Err(err) => {
                            if let Some(d) = dialog_weak.upgrade() {
                                d.update(cx, |dlg, cx| dlg.set_error(err, cx));
                            }
                        }
                    }
                }
            });
        });
        self.folder_dialog = Some(dialog);
        self.active_modal = ActiveModal::Folder;
        self.modal_state.update(cx, |modal, cx| modal.open(cx));
    }

    fn enter_folder(&mut self, folder_id: SharedString, cx: &mut Context<Self>) {
        self.current_folder = Some(folder_id);
        self.selected_ids.clear();
        self.search_input.update(cx, |input, cx| input.clear(cx));
        self.search_query = SharedString::new_static("");
        cx.notify();
    }

    fn exit_folder(&mut self, cx: &mut Context<Self>) {
        self.current_folder = None;
        self.selected_ids.clear();
        cx.notify();
    }

    fn close_header_menu(&mut self, cx: &mut Context<Self>) {
        self.header_menu = None;
        self.header_menu_position = None;
        self.header_menu_subscription.take();
        cx.notify();
    }

    fn open_header_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.header_menu.is_some() {
            self.close_header_menu(cx);
            return;
        }
        let cfg = SshGlobalConfig::load();
        let folder_sort = cfg.folder_sort;
        let host_sort = cfg.host_sort;
        let is_root = self.current_folder.is_none();
        let host_view = cx.entity().clone();
        let host_view2 = host_view.clone();
        let current_folder_clone = self.current_folder.clone();
        let menu =
            PopupMenu::build(window, cx, move |menu, _window, _cx| {
                let mut m = menu;
                m = m.item(
                    PopupMenuItem::new("新建主机")
                        .icon(crate::components::button::Icon::new("icons/plus.svg"))
                        .on_click({
                            let host_view = host_view.clone();
                            let current_folder_clone = current_folder_clone.clone();
                            move |_, window, cx| {
                                let target = current_folder_clone.clone().map(|s| s.to_string());
                                host_view.update(cx, |this, cx| {
                                    this.close_header_menu(cx);
                                    this.open_host_modal(target, window, cx);
                                });
                            }
                        }),
                );
                m = m.item(
                    PopupMenuItem::new("新建文件夹")
                        .icon(crate::components::button::Icon::new(
                            "icons/folder.badge.plus.svg",
                        ))
                        .on_click({
                            let host_view = host_view2.clone();
                            move |_, window, cx| {
                                host_view.update(cx, |this, cx| {
                                    this.close_header_menu(cx);
                                    this.open_create_folder_dialog(window, cx);
                                });
                            }
                        }),
                );
                m = m.separator();
                m = m.item(PopupMenuItem::label("排序方式"));
                let sort_options = [
                    (
                        "名称 A → Z",
                        SortMode::NameAsc,
                        if is_root {
                            folder_sort == SortMode::NameAsc
                        } else {
                            host_sort == SortMode::NameAsc
                        },
                    ),
                    (
                        "名称 Z → A",
                        SortMode::NameDesc,
                        if is_root {
                            folder_sort == SortMode::NameDesc
                        } else {
                            host_sort == SortMode::NameDesc
                        },
                    ),
                    (
                        "创建时间 新→旧",
                        SortMode::CreatedDesc,
                        if is_root {
                            folder_sort == SortMode::CreatedDesc
                        } else {
                            host_sort == SortMode::CreatedDesc
                        },
                    ),
                    (
                        "创建时间 旧→新",
                        SortMode::CreatedAsc,
                        if is_root {
                            folder_sort == SortMode::CreatedAsc
                        } else {
                            host_sort == SortMode::CreatedAsc
                        },
                    ),
                ];
                for (label, mode, checked) in sort_options {
                    let host_view = host_view.clone();
                    m = m.item(PopupMenuItem::new(label).checked(checked).on_click(
                        move |_, _, cx| {
                            host_view.update(cx, |_, cx| {
                                let mut cfg = SshGlobalConfig::load();
                                if is_root {
                                    cfg.folder_sort = mode;
                                    cfg.host_sort = mode;
                                } else {
                                    cfg.host_sort = mode;
                                }
                                let _ = cfg.save();
                                cx.notify();
                            });
                        },
                    ));
                }
                m
            });

        let sub = cx.subscribe(&menu, |this, _, _: &DismissEvent, cx| {
            this.close_header_menu(cx);
        });
        self.header_menu = Some(menu);
        self.header_menu_position = Some(window.mouse_position());
        self.header_menu_subscription = Some(sub);
        cx.notify();
    }

    pub fn build_modal(&mut self, cx: &mut Context<Self>) -> Option<Modal> {
        let modal_state = self.modal_state.read(cx);
        if !modal_state.is_rendered() {
            return None;
        }
        let entity = cx.entity();
        let (child, width) = match self.active_modal {
            ActiveModal::Host => {
                let form = self.host_form.as_ref()?.clone();
                (form.into_any_element(), px(540.))
            }
            ActiveModal::Folder => {
                let dlg = self.folder_dialog.as_ref()?.clone();
                (dlg.into_any_element(), px(400.))
            }
            ActiveModal::None => return None,
        };
        Some(
            Modal::new("host-modal")
                .width(width)
                .max_height(px(700.))
                .top_padding(px(120.))
                .closing(modal_state.is_closing())
                .epoch(modal_state.epoch())
                .on_close(move |_window, cx| {
                    entity.update(cx, |this, cx| {
                        this.modal_state.update(cx, |modal, cx| modal.close(cx));
                        this.active_modal = ActiveModal::None;
                    });
                })
                .child(child),
        )
    }

    fn render_header(
        &self,
        title: String,
        cx: &mut Context<Self>,
        theme: crate::theme::UiTheme,
    ) -> gpui::AnyElement {
        div()
            .w_full()
            .h(px(36.0))
            .flex()
            .items_center()
            .justify_between()
            .px(px(4.0))
            .mb(px(8.0))
            .child(
                round_icon_button(theme, Icon::ChevronLeft, 8.0)
                    .id("host-back-btn")
                    .on_click(cx.listener(|this, _, window, cx| {
                        if this.current_folder.is_some() {
                            this.exit_folder(cx);
                        } else if let Some(ref f) = this.on_select.clone() {
                            f("__back", window, cx);
                        }
                    })),
            )
            .child(
                div()
                    .text_sm()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.text)
                    .child(title),
            )
            .child(
                round_icon_button(theme, Icon::Ellipsis, 14.0)
                    .id("host-header-more")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_header_menu(window, cx);
                    })),
            )
            .into_any_element()
    }

    fn render_folder_row(
        &self,
        group: &HostGroup,
        count: usize,
        cx: &mut Context<Self>,
        theme: crate::theme::UiTheme,
        _is_root: bool,
    ) -> gpui::AnyElement {
        let folder_id = group.id.clone();
        let folder_id_shared: SharedString = folder_id.clone().into();
        let name = group.name.clone();
        let subtitle = folder_subtitle(count);
        // folder icon bg: use accent
        let row = div()
            .id(SharedString::from(format!("folder-item-{}", folder_id)))
            .w_full()
            .h(px(52.0))
            .px(px(10.0))
            .flex()
            .items_center()
            .gap(px(10.0))
            .rounded(px(8.0))
            .cursor_pointer()
            .hover(|style| style.bg(hover_bg(theme)))
            .text_color(theme.text)
            .child(
                div()
                    .w(px(32.0))
                    .h(px(32.0))
                    .rounded(px(8.0))
                    .bg(theme.accent)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        gpui::svg()
                            .size(px(16.0))
                            .path(Icon::Folder.path())
                            .text_color(theme.accent_foreground),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .justify_center()
                    .gap(px(2.0))
                    .child(
                        div()
                            .text_size(px(12.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(name.clone()),
                    )
                    .child(
                        div()
                            .text_size(px(10.0))
                            .line_height(px(14.0))
                            .text_color(theme.text_muted)
                            .child(subtitle),
                    ),
            )
            .child(
                gpui::svg()
                    .size(px(10.0))
                    .path(Icon::ChevronRight.path())
                    .text_color(theme.text_muted),
            )
            .on_click(cx.listener({
                let fid = folder_id_shared.clone();
                move |this, _, _, cx| {
                    this.enter_folder(fid.clone(), cx);
                }
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener({
                    let fid = folder_id.clone();
                    move |this, _, _, cx| {
                        this.context_menu_folder_id = Some(fid.clone().into());
                        this.context_menu_host_id = None;
                        cx.notify();
                    }
                }),
            )
            .context_menu(format!("folder-menu-{}", folder_id), {
                let entity = cx.entity().clone();
                let fid = folder_id.clone();
                let count = count;
                move |menu: PopupMenu, _window, _cx| {
                    let fid_clone = fid.clone();
                    let fid2 = fid.clone();
                    let fid3 = fid.clone();
                    let entity_clone = entity.clone();
                    let entity_clone2 = entity.clone();
                    let entity_clone3 = entity.clone();
                    let entity_clone4 = entity.clone();
                    menu.item(
                        PopupMenuItem::new(if count == 0 { "连接该组".to_string() } else { format!("连接该组 ({}台)", count) })
                            .icon(crate::components::button::Icon::new("icons/link.svg"))
                            .on_click({
                                let entity_clone = entity_clone.clone();
                                let fid_clone = fid_clone.clone();
                                move |_, window, cx| {
                                    let fid = fid_clone.clone();
                                    entity_clone.update(cx, |this, cx| {
                                        let (_, configs, _) = load_grouped_hosts();
                                        let on_connect = this.on_connect.clone();
                                        for cfg in configs.iter().filter(|c| c.group_id.as_deref() == Some(&fid)) {
                                            if let Some(ref f) = on_connect {
                                                f(cfg, window, cx);
                                            }
                                        }
                                    });
                                }
                            }),
                    )
                    .separator()
                    .item(
                        PopupMenuItem::new("重命名")
                            .icon(crate::components::button::Icon::new("icons/edit.svg"))
                            .on_click({
                                let entity_clone2 = entity_clone2.clone();
                                let fid2 = fid2.clone();
                                move |_, window, cx| {
                                    entity_clone2.update(cx, |this, cx| {
                                        this.open_rename_folder_dialog(fid2.clone(), window, cx);
                                    });
                                }
                            }),
                    )
                    .item(
                        PopupMenuItem::new("删除文件夹")
                            .icon(crate::components::button::Icon::new("icons/trash.svg"))
                            .on_click({
                                let entity_clone3 = entity_clone3.clone();
                                let fid3 = fid3.clone();
                                move |_, window, cx| {
                                    let fid = fid3.clone();
                                    let weak = entity_clone3.downgrade();
                                    let prompt = window.prompt(
                                        gpui::PromptLevel::Warning,
                                        "删除文件夹",
                                        Some("确定删除文件夹？\n\n选择「仅删除文件夹」会将其中主机移至未分组，选择「删除文件夹及主机」会一并删除。"),
                                        &["仅删除文件夹", "删除文件夹及主机", "取消"],
                                        cx,
                                    );
                                    cx.spawn(async move |cx| {
                                        if let Ok(choice) = prompt.await {
                                            if choice == 2 { return; }
                                            let delete_hosts = choice == 1;
                                            if let Some(ent) = weak.upgrade() {
                                                ent.update(cx, |this, cx| {
                                                    let mut cfg = SshGlobalConfig::load();
                                                    let _ = cfg.delete_group(&fid, delete_hosts);
                                                    let _ = cfg.save();
                                                    if this.current_folder.as_deref() == Some(&fid) {
                                                        this.current_folder = None;
                                                    }
                                                    this.context_menu_folder_id = None;
                                                    cx.notify();
                                                });
                                            }
                                        }
                                    }).detach();
                                }
                            }),
                    )
                    .separator()
                    .item(
                        PopupMenuItem::new("在此文件夹新建主机")
                            .icon(crate::components::button::Icon::new("icons/plus.svg"))
                            .on_click({
                                let entity_clone4 = entity_clone4.clone();
                                let fid = fid.clone();
                                move |_, window, cx| {
                                    let fid = fid.clone();
                                    entity_clone4.update(cx, |this, cx| {
                                        this.open_host_modal(Some(fid), window, cx);
                                    });
                                }
                            }),
                    )
                }
            });
        row.into_any_element()
    }

    fn render_host_row(
        &self,
        host: &Host,
        configs: &Rc<Vec<SshConnectionConfig>>,
        groups: &Rc<Vec<HostGroup>>,
        cx: &mut Context<Self>,
        theme: crate::theme::UiTheme,
        on_select: &OptionalStrCallback,
        on_connect: &Option<HostConfigCallback>,
    ) -> gpui::AnyElement {
        let icon_color = host.icon_color.to_hsla(cx);
        let host_id = host.id.to_string();
        let is_selected = self.selected_ids.iter().any(|s| s.as_ref() == host_id);
        let host_id_clone = host_id.clone();
        div()
            .id(SharedString::from(format!("host-item-{host_id}")))
            .w_full()
            .h(px(52.0))
            .px(px(10.0))
            .flex()
            .items_center()
            .gap(px(10.0))
            .rounded(px(8.0))
            .cursor_pointer()
            .map(row_selection_state(theme, is_selected))
            .child(
                div()
                    .w(px(32.0))
                    .h(px(32.0))
                    .rounded(px(8.0))
                    .bg(if is_selected {
                        theme.accent_foreground
                    } else {
                        icon_color
                    })
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        gpui::svg()
                            .size(px(16.0))
                            .path(host.os_icon.unwrap_or(Icon::Server).path())
                            .text_color(if is_selected {
                                theme.accent
                            } else {
                                theme.accent_foreground
                            }),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .justify_center()
                    .gap(px(2.0))
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .text_color(if is_selected {
                                        theme.accent_foreground
                                    } else {
                                        theme.text
                                    })
                                    .child(host.name.to_string()),
                            ),
                    )
                    .child(
                        div()
                            .w_full()
                            .min_w_0()
                            .text_size(px(10.0))
                            .line_height(px(14.0))
                            .text_color(if is_selected {
                                theme.accent_foreground.opacity(0.7)
                            } else {
                                theme.text_muted
                            })
                            .child(host.subtitle()),
                    ),
            )
            .on_click(cx.listener({
                let on_select = on_select.clone();
                let on_connect = on_connect.clone();
                // 只搬 `Rc`（引用计数 +1）：双击才需要的那份配置在事件里按 id
                // 现查现克隆，不让每一行每次重绘都深拷贝一遍连接配置。
                let configs = configs.clone();
                let id = host_id.clone();
                move |this, event: &gpui::ClickEvent, window, cx| {
                    if event.modifiers().secondary() {
                        let id_shared: SharedString = id.clone().into();
                        if this.selected_ids.contains(&id_shared) {
                            this.selected_ids.remove(&id_shared);
                        } else {
                            this.selected_ids.insert(id_shared);
                        }
                        cx.notify();
                        return;
                    }
                    let is_double_click = event.click_count() >= 2;
                    this.selected_ids.clear();
                    this.selected_ids.insert(id.clone().into());
                    cx.notify();
                    if !is_double_click {
                        if let Some(ref f) = on_select {
                            f(&id, window, cx);
                        }
                    } else if let Some(ref f) = on_connect
                        && let Some(config) = configs.iter().find(|c| c.id() == id)
                    {
                        f(config, window, cx);
                    }
                }
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener({
                    let host_id = host_id_clone.clone();
                    move |this, _, _, cx| {
                        let id_shared: SharedString = host_id.clone().into();
                        this.context_menu_host_id = Some(id_shared.clone());
                        this.context_menu_folder_id = None;
                        if !this.selected_ids.contains(&id_shared) {
                            this.selected_ids.clear();
                            this.selected_ids.insert(id_shared);
                        }
                        cx.notify();
                    }
                }),
            )
            .context_menu(format!("host-item-menu-{}", host_id), {
                let entity = cx.entity().clone();
                let host_id_for_menu = host_id.clone();
                // 菜单回调持有的是共享句柄（`Rc` +1），不是整份分组表的深拷贝：
                // 原先每个可见行、每一帧都要把全部分组连名字一起克隆一遍。
                let groups_clone = groups.clone();
                let host_group_id = host.group_id.clone();
                move |menu: PopupMenu, _window, _cx| {
                    let count = entity.read(_cx).selected_ids.len();
                    let mut m =
                        connect_menu_items(menu, count, crate::components::button::Icon::new);
                    use crate::components::button::Icon as MenuIcon;
                    if count == 1 {
                        m = m
                            .menu_with_icon(
                                "编辑",
                                MenuIcon::new("icons/edit.svg"),
                                Box::new(EditHost),
                            )
                            .menu_with_icon(
                                "拷贝地址",
                                MenuIcon::new("icons/document.on.document.svg"),
                                Box::new(CopyHostAddress),
                            )
                            .separator();
                    }
                    // 移动到文件夹 — 支持单选与 Cmd 多选批量移动
                    {
                        let entity_clone = entity.clone();
                        let host_id_clone2 = host_id_for_menu.clone();
                        let groups_for_move = groups_clone.clone();
                        let is_single = count == 1;
                        // 单选时隐藏当前所在分组，避免冗余；多选时全部显示便于批量归集
                        let is_ungrouped = host_group_id.is_none();
                        let mut has_move_items = false;
                        if !is_single || !is_ungrouped {
                            let entity_c = entity_clone.clone();
                            let hid = host_id_clone2.clone();
                            m = m.item(
                                PopupMenuItem::new("移动到「未分组」")
                                    .icon(MenuIcon::new("icons/folder.svg"))
                                    .on_click(move |_, _, cx| {
                                        let hid = hid.clone();
                                        entity_c.update(cx, |this, cx| {
                                            let ids: Vec<String> = if this.selected_ids.is_empty() {
                                                vec![hid.clone()]
                                            } else {
                                                this.selected_ids
                                                    .iter()
                                                    .map(|s| s.to_string())
                                                    .collect()
                                            };
                                            let mut cfg = SshGlobalConfig::load();
                                            cfg.move_hosts_to_group(&ids, None);
                                            let _ = cfg.save();
                                            this.context_menu_host_id = None;
                                            cx.notify();
                                        });
                                    }),
                            );
                            has_move_items = true;
                        }
                        for g in groups_for_move.iter() {
                            if is_single && Some(g.id.as_str()) == host_group_id.as_deref() {
                                continue;
                            }
                            let g_id = g.id.clone();
                            let g_name = g.name.clone();
                            let entity_c = entity_clone.clone();
                            let hid = host_id_clone2.clone();
                            let label = format!("移动到「{}」", g_name);
                            m = m.item(
                                PopupMenuItem::new(label)
                                    .icon(MenuIcon::new("icons/folder.svg"))
                                    .on_click(move |_, _, cx| {
                                        let hid = hid.clone();
                                        let gid = g_id.clone();
                                        entity_c.update(cx, |this, cx| {
                                            let ids: Vec<String> = if this.selected_ids.is_empty() {
                                                vec![hid.clone()]
                                            } else {
                                                this.selected_ids
                                                    .iter()
                                                    .map(|s| s.to_string())
                                                    .collect()
                                            };
                                            let mut cfg = SshGlobalConfig::load();
                                            cfg.move_hosts_to_group(&ids, Some(&gid));
                                            let _ = cfg.save();
                                            this.context_menu_host_id = None;
                                            cx.notify();
                                        });
                                    }),
                            );
                            has_move_items = true;
                        }
                        if has_move_items {
                            m = m.separator();
                        }
                    }
                    let label = if count == 1 {
                        "删除".to_string()
                    } else {
                        format!("删除 {} 台主机", count)
                    };
                    m.menu_with_icon(
                        &label,
                        MenuIcon::new("icons/trash.svg"),
                        Box::new(DeleteHost),
                    )
                }
            })
            .into_any_element()
    }
}

impl HostView {
    /// 从共享配置/钥匙串快照重建列表与排序缓存。
    fn build_grouped_cache(
        config: Arc<SshGlobalConfig>,
        keychain: Arc<KeychainStore>,
    ) -> GroupedHostsCache {
        let (hosts, configs, mut groups) = load_grouped_hosts_from(&config, &keychain);
        let mut host_counts: HashMap<String, usize> = HashMap::new();
        for host in &hosts {
            if let Some(group_id) = host.group_id.as_ref() {
                *host_counts.entry(group_id.to_string()).or_insert(0) += 1;
            }
        }
        sort_host_groups(&mut groups, config.folder_sort, Some(&host_counts));
        GroupedHostsCache {
            folder_sort: config.folder_sort,
            host_sort: config.host_sort,
            config,
            keychain,
            hosts: Rc::new(hosts),
            configs: Rc::new(configs),
            groups: Rc::new(groups),
            host_counts,
        }
    }
}

crate::impl_focusable!(HostView);

impl Render for HostView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().ui;
        // 配置/钥匙串按 `Arc` 身份缓存：文件未变时同一帧复用列表，
        // 避免每帧 `load_grouped_hosts` + 深拷贝配置。
        let config = SshGlobalConfig::load_shared();
        let keychain = KeychainStore::load_shared_or_default();
        let needs_rebuild = self.grouped_cache.as_ref().is_none_or(|cache| {
            !Arc::ptr_eq(&cache.config, &config)
                || !Arc::ptr_eq(&cache.keychain, &keychain)
                || cache.folder_sort != config.folder_sort
                || cache.host_sort != config.host_sort
        });
        if needs_rebuild {
            self.grouped_cache = Some(Self::build_grouped_cache(config, keychain));
        }
        let cache = self
            .grouped_cache
            .as_ref()
            .expect("grouped cache built above");
        let hosts = cache.hosts.clone();
        let configs = cache.configs.clone();
        let groups = cache.groups.clone();
        let host_counts = &cache.host_counts;
        let host_sort = cache.host_sort;

        let on_select = self.on_select.clone();
        let on_connect = self.on_connect.clone();

        // 标题
        let title = if let Some(ref fid) = self.current_folder {
            groups
                .iter()
                .find(|g| g.id == *fid.as_ref())
                .map(|g| g.name.clone())
                .unwrap_or_else(|| "文件夹".to_string())
        } else {
            "Host".to_string()
        };
        let header = self.render_header(title, cx, theme);

        // 搜索框
        let search_focused = self
            .search_input
            .read(cx)
            .focus_handle(cx)
            .is_focused(window);
        let search_box = div()
            .w_full()
            .h(px(32.0))
            .flex()
            .items_center()
            .px(px(10.0))
            .gap(px(6.0))
            .rounded(px(12.0))
            .bg(theme.tab_hover_background)
            .when(search_focused, |this| {
                this.border(px(3.0)).border_color(theme.accent.opacity(0.5))
            })
            .mb(px(12.0))
            .child(
                gpui::svg()
                    .size(px(14.0))
                    .path(Icon::MagnifyingGlass.path())
                    .text_color(theme.text_muted),
            )
            .child(EditorInput::new(&self.search_input).transparent());

        let query = self.search_query.as_ref().to_lowercase();
        let query_str = query.as_str();

        // 构建列表
        let mut list = div().w_full().flex().flex_col().gap(px(2.0));

        if self.current_folder.is_none() {
            // 根列表：文件夹 + 未分组主机
            let filtered_groups: Vec<&HostGroup> = groups
                .iter()
                .filter(|g| query_str.is_empty() || Self::folder_matches_query(g, query_str))
                .collect();
            for g in filtered_groups {
                let cnt = host_counts.get(&g.id).copied().unwrap_or(0);
                // 若搜索且文件夹名不匹配但组内无匹配主机，是否隐藏？简化：只要文件夹名匹配即显示
                // 若 query 非空且文件夹名不匹配，检查组内是否有匹配主机，有则仍显示
                if !query_str.is_empty() && !Self::folder_matches_query(g, query_str) {
                    let has_matching_host = hosts.iter().any(|h| {
                        h.group_id.as_deref() == Some(g.id.as_str())
                            && Self::host_matches_query(h, query_str)
                    });
                    if !has_matching_host {
                        continue;
                    }
                }
                list = list.child(self.render_folder_row(g, cnt, cx, theme, true));
            }

            // 未分组主机
            let mut ungrouped: Vec<Host> = hosts
                .iter()
                .filter(|h| h.group_id.is_none())
                .filter(|h| Self::host_matches_query(h, query_str))
                .cloned()
                .collect();
            sort_hosts(&mut ungrouped, host_sort);

            if !ungrouped.is_empty() && !groups.is_empty() {
                // 分隔线 + 未分组标题
                let has_folders = !groups.is_empty();
                if has_folders {
                    list = list.child(
                        div()
                            .w_full()
                            .h(px(1.0))
                            .bg(theme.border.opacity(0.5))
                            .my(px(6.0)),
                    );
                    list = list.child(
                        div()
                            .w_full()
                            .px(px(6.0))
                            .py(px(2.0))
                            .text_xs()
                            .text_color(theme.text_muted)
                            .child("未分组"),
                    );
                }
            }

            if ungrouped.is_empty() && query_str.is_empty() && groups.is_empty() {
                list = list.child(
                    div()
                        .w_full()
                        .py(px(24.0))
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap(px(6.0))
                        .text_sm()
                        .text_color(theme.text_muted)
                        .child("暂无主机")
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.text_muted.opacity(0.8))
                                .child("点击右上角 ⋯ 新建主机或文件夹"),
                        ),
                );
            } else if ungrouped.is_empty() && !query_str.is_empty() && groups.is_empty() {
                list = list.child(
                    div()
                        .w_full()
                        .py(px(16.0))
                        .text_sm()
                        .text_color(theme.text_muted)
                        .child("未找到匹配的主机或文件夹"),
                );
            }

            for host in &ungrouped {
                list = list.child(self.render_host_row(
                    host,
                    &configs,
                    &groups,
                    cx,
                    theme,
                    &on_select,
                    &on_connect,
                ));
            }
        } else {
            // 文件夹内页
            // 关键防御：用 match 替代 unwrap，文件夹被删除/导航竞态时
            // 降级为空列表而非 panic，保证侧边栏渲染永不崩溃。
            let fid_opt = self.current_folder.clone();
            let fid = match fid_opt {
                Some(v) => v,
                None => {
                    // 竞态兜底：视为空文件夹，渲染“暂无主机”占位。
                    gpui::SharedString::new("")
                }
            };
            let mut folder_hosts: Vec<Host> = hosts
                .iter()
                .filter(|h| h.group_id.as_deref() == Some(fid.as_ref()))
                .filter(|h| Self::host_matches_query(h, query_str))
                .cloned()
                .collect();
            sort_hosts(&mut folder_hosts, host_sort);

            if folder_hosts.is_empty() {
                if query_str.is_empty() {
                    list = list.child(
                        div()
                            .w_full()
                            .py(px(24.0))
                            .flex()
                            .flex_col()
                            .items_center()
                            .gap(px(6.0))
                            .text_sm()
                            .text_color(theme.text_muted)
                            .child("暂无主机")
                            .child(div().text_xs().child("点击右上角 ⋯ 新建主机")),
                    );
                } else {
                    list = list.child(
                        div()
                            .w_full()
                            .py(px(16.0))
                            .text_sm()
                            .text_color(theme.text_muted)
                            .child("未找到匹配的主机"),
                    );
                }
            }
            for host in &folder_hosts {
                list = list.child(self.render_host_row(
                    host,
                    &configs,
                    &groups,
                    cx,
                    theme,
                    &on_select,
                    &on_connect,
                ));
            }
        }

        let header_menu_el = if let Some(menu) = self.header_menu.clone() {
            let pos = self.header_menu_position.unwrap_or(point(px(0.), px(0.)));
            deferred(
                anchored()
                    .position(pos)
                    .anchor(gpui::Anchor::TopRight)
                    .snap_to_window_with_margin(px(8.))
                    .child(menu),
            )
            .with_priority(100)
            .into_any_element()
        } else {
            div().into_any_element()
        };

        let configs_rc = configs;
        let on_connect_action = self.on_connect.clone();
        let on_connect_via_proxy = self.on_connect.clone();
        let hosts_for_delete = hosts.clone();

        div()
            .w_full()
            .h_full()
            .flex()
            .flex_col()
            .on_action(cx.listener({
                let configs_rc = configs_rc.clone();
                move |this, _: &ConnectHost, window, cx| {
                    let picked = selected_configs(&configs_rc, &this.selected_ids, false);
                    if let Some(ref f) = on_connect_action {
                        for config in &picked {
                            f(config, window, cx);
                        }
                    }
                }
            }))
            .on_action(cx.listener({
                let configs_rc = configs_rc.clone();
                move |this, _: &ConnectHostViaProxy, window, cx| {
                    let picked = selected_configs(&configs_rc, &this.selected_ids, true);
                    if let Some(ref f) = on_connect_via_proxy {
                        for config in &picked {
                            f(config, window, cx);
                        }
                    }
                }
            }))
            .on_action(cx.listener({
                let configs_rc = configs_rc.clone();
                move |this, _: &EditHost, window, cx| {
                    let host_id = this
                        .context_menu_host_id
                        .clone()
                        .or_else(|| this.selected_ids.iter().next().cloned());
                    if let Some(id) = host_id
                        && let Some(config) =
                            configs_rc.iter().find(|c| c.id() == id.as_ref()).cloned()
                    {
                        this.open_edit_modal(&config, window, cx);
                    }
                }
            }))
            .on_action(cx.listener({
                let configs_rc = configs_rc.clone();
                move |this, _: &CopyHostAddress, _window, cx| {
                    let host_id = this
                        .context_menu_host_id
                        .clone()
                        .or_else(|| this.selected_ids.iter().next().cloned());
                    if let Some(id) = host_id
                        && let Some(config) = configs_rc.iter().find(|c| c.id() == id.as_ref())
                    {
                        cx.write_to_clipboard(ClipboardItem::new_string(config.host.clone()));
                    }
                }
            }))
            .on_action(cx.listener(move |this, _: &DeleteHost, window, cx| {
                let host_ids: Vec<_> = this.selected_ids.iter().cloned().collect();
                if host_ids.is_empty() {
                    return;
                }
                let count = host_ids.len();
                let names: Vec<_> = host_ids
                    .iter()
                    .filter_map(|id| {
                        hosts_for_delete
                            .iter()
                            .find(|h| h.id.as_ref() == id.as_ref())
                            .map(|h| h.name.to_string())
                    })
                    .collect();
                let message = if count == 1 {
                    if let Some(name) = names.first() {
                        format!("确定要删除主机 '{}' 吗？此操作不可撤销。", name)
                    } else {
                        "确定要删除该主机吗？此操作不可撤销。".to_string()
                    }
                } else {
                    format!(
                        "确定要删除选中的 {} 个主机（{}）吗？此操作不可撤销。",
                        count,
                        names.join("、")
                    )
                };
                let weak = cx.weak_entity();
                let ids_to_delete = host_ids.clone();
                let answer = window.prompt(
                    gpui::PromptLevel::Warning,
                    "确认删除",
                    Some(&message),
                    &["删除", "取消"],
                    cx,
                );
                cx.spawn(async move |_, cx| {
                    if let Ok(0) = answer.await
                        && let Some(entity) = weak.upgrade()
                    {
                        entity.update(cx, |this, cx| {
                            if let Ok(mut global) = SshGlobalConfig::load_or_create() {
                                for id in &ids_to_delete {
                                    global.remove_connection(id);
                                }
                                let _ = global.save();
                            }
                            this.selected_ids.clear();
                            this.context_menu_host_id = None;
                            cx.notify();
                        });
                    }
                })
                .detach();
            }))
            .child(header)
            .child(header_menu_el)
            .child(search_box)
            .child(
                div()
                    .id("host-scroll")
                    .track_scroll(&self.scroll_handle)
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .child(list),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ssh::config::AuthMethod;

    /// 从菜单里取出「标签 → 动作名」的映射，供接线断言使用。
    fn menu_entries(menu: &PopupMenu) -> Vec<(String, Option<String>)> {
        menu.menu_items
            .iter()
            .filter_map(|item| match item {
                PopupMenuItem::Item { label, action, .. } => Some((
                    label.to_string(),
                    action.as_ref().map(|action| action.name().to_string()),
                )),
                _ => None,
            })
            .collect()
    }

    /// 两个连接项必须各挂各的动作。
    ///
    /// 断言的是 `PopupMenu` 里的真实条目而不是文案常量：动作接反了界面上完全
    /// 看不出来，只有连上海外主机才会发现代理没生效（或本机连接被误送进代理）。
    #[gpui::test]
    fn connect_menu_items_wire_direct_and_proxy_actions(cx: &mut gpui::TestAppContext) {
        let (single, multi) = cx.update(|cx| {
            let single =
                connect_menu_items(PopupMenu::new(cx), 1, crate::components::button::Icon::new);
            let multi =
                connect_menu_items(PopupMenu::new(cx), 3, crate::components::button::Icon::new);
            (menu_entries(&single), menu_entries(&multi))
        });

        assert_eq!(
            single,
            vec![
                (
                    "连接".to_string(),
                    Some("host_actions::ConnectHost".to_string())
                ),
                (
                    "使用系统代理连接".to_string(),
                    Some("host_actions::ConnectHostViaProxy".to_string())
                ),
            ],
            "单选菜单：第一项直连、第二项经系统代理"
        );
        assert_eq!(
            multi,
            vec![
                (
                    "连接 3 台主机".to_string(),
                    Some("host_actions::ConnectHost".to_string())
                ),
                (
                    "使用系统代理连接 3 台主机".to_string(),
                    Some("host_actions::ConnectHostViaProxy".to_string())
                ),
            ],
            "多选菜单：文案带数量，动作保持不变"
        );
    }

    /// 「使用系统代理连接」的菜单项：图标必须真实存在于嵌入资源里。
    ///
    /// gpui 画不出图标时不报错、不 panic，菜单上只是空一块——所以直接查
    /// `Assets::get`，而不是只断言路径字符串长得对（与 tab_bar 的 agent 图标
    /// 用例同款）。
    #[test]
    fn proxy_menu_icon_is_an_embedded_asset() {
        assert!(
            crate::assets::Assets::get("icons/network.svg").is_some(),
            "「使用系统代理连接」的图标不在嵌入资源里"
        );
    }

    /// `selected_configs`：选中哪些就连哪些，标记随调用路径而变。
    ///
    /// 覆盖单选、多选（Cmd 多选批量连接）、以及未选中任何主机三种情形——
    /// 「连错主机」与「代理没生效」都从这里开始。
    #[test]
    fn selected_configs_carry_the_proxy_flag_only_on_the_proxy_path() {
        let a = SshConnectionConfig::new("a.example.com", "root", AuthMethod::Agent);
        let b = SshConnectionConfig::new("b.example.com", "root", AuthMethod::Agent);
        let c = SshConnectionConfig::new("c.example.com", "root", AuthMethod::Agent);
        let configs = vec![a.clone(), b.clone(), c.clone()];
        let ids = |list: &[&SshConnectionConfig]| -> HashSet<SharedString> {
            list.iter().map(|c| SharedString::from(c.id())).collect()
        };

        // 单选 + 直连
        let picked = selected_configs(&configs, &ids(&[&a]), false);
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].host, "a.example.com");
        assert!(!picked[0].use_system_proxy, "直连路径不该带代理标记");

        // 单选 + 经代理
        let picked = selected_configs(&configs, &ids(&[&b]), true);
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].host, "b.example.com");
        assert!(picked[0].use_system_proxy, "代理路径必须带代理标记");

        // 多选 + 经代理：每一台都要带标记（漏一台就会有一台悄悄直连）
        let picked = selected_configs(&configs, &ids(&[&a, &c]), true);
        assert_eq!(picked.len(), 2, "选中的两台都要连");
        assert!(picked.iter().all(|c| c.use_system_proxy));

        // 未选中：什么都不连
        assert!(selected_configs(&configs, &HashSet::new(), true).is_empty());
    }
}
