//! AppKit window: a search field, a filter, a refresh button, and an NSOutlineView with one
//! row per computer and its agents as children. Copy with a double click, ⌘C or the Edit
//! menu; the line under the list always says what just happened.

use crate::model::{self, Row, COLUMNS, FILTERS};
use fmesh_core::view::View;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject, Sel};
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate, NSAutoresizingMaskOptions, NSBackingStoreType,
    NSButton, NSColor, NSControlTextEditingDelegate, NSFont, NSMenu, NSMenuItem, NSOutlineView, NSOutlineViewDataSource, NSOutlineViewDelegate,
    NSPasteboard, NSPasteboardTypeString, NSScrollView, NSSearchField, NSSegmentedControl, NSSegmentSwitchTracking,
    NSTableColumn, NSTableViewDelegate, NSTableViewDataSource, NSTextField, NSWindow, NSWindowDelegate, NSWindowStyleMask,
};
use objc2_foundation::{
    ns_string, MainThreadMarker, NSArray, NSInteger, NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize,
    NSString,
};
use std::cell::{Cell, OnceCell, RefCell};
use std::collections::HashMap;

const W: f64 = 980.0;
const H: f64 = 620.0;

#[derive(Default)]
struct Ivars {
    window: OnceCell<Retained<NSWindow>>,
    outline: OnceCell<Retained<NSOutlineView>>,
    search: OnceCell<Retained<NSSearchField>>,
    filter: OnceCell<Retained<NSSegmentedControl>>,
    refresh: OnceCell<Retained<NSButton>>,
    summary: OnceCell<Retained<NSTextField>>,
    hint: OnceCell<Retained<NSTextField>>,
    view: RefCell<Option<View>>,
    shown: RefCell<Vec<usize>>,
    /// One stable object per row, so NSOutlineView keeps expansion across searches.
    keys: RefCell<HashMap<Row, Retained<NSString>>>,
    loading: Cell<bool>,
}

thread_local! {
    static APP: OnceCell<Retained<Delegate>> = const { OnceCell::new() };
}

fn key_row(item: Option<&AnyObject>) -> Option<Row> {
    let s = item?.downcast_ref::<NSString>()?.to_string();
    let (d, a) = match s.split_once(':') {
        Some((d, a)) => (d.parse().ok()?, Some(a.parse().ok()?)),
        None => (s.parse().ok()?, None),
    };
    Some(Row { device: d, agent: a })
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements and Delegate does not implement Drop.
    #[unsafe(super = NSObject)]
    #[thread_kind = MainThreadOnly]
    #[ivars = Ivars]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl NSApplicationDelegate for Delegate {
        #[unsafe(method(applicationDidFinishLaunching:))]
        fn did_finish_launching(&self, _n: &NSNotification) {
            self.build();
            self.reload();
        }
    }

    unsafe impl NSWindowDelegate for Delegate {
        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _n: &NSNotification) {
            // Closing the list ends the process: nothing stays resident.
            NSApplication::sharedApplication(self.mtm()).terminate(None);
        }
    }

    unsafe impl NSControlTextEditingDelegate for Delegate {}
    unsafe impl NSTableViewDataSource for Delegate {}
    unsafe impl NSTableViewDelegate for Delegate {}

    unsafe impl NSOutlineViewDataSource for Delegate {
        #[unsafe(method(outlineView:numberOfChildrenOfItem:))]
        fn children(&self, _o: &NSOutlineView, item: Option<&AnyObject>) -> NSInteger {
            let view = self.ivars().view.borrow();
            let Some(v) = view.as_ref() else { return 0 };
            match key_row(item) {
                None => self.ivars().shown.borrow().len() as NSInteger,
                Some(Row { device, agent: None }) => v.devices[device].agent_list().len() as NSInteger,
                Some(_) => 0,
            }
        }

        #[unsafe(method_id(outlineView:child:ofItem:))]
        fn child(&self, _o: &NSOutlineView, index: NSInteger, item: Option<&AnyObject>) -> Retained<AnyObject> {
            let row = match key_row(item) {
                None => Row { device: self.ivars().shown.borrow()[index as usize], agent: None },
                Some(Row { device, .. }) => Row { device, agent: Some(index as usize) },
            };
            Retained::into_super(Retained::into_super(self.key(row)))
        }

        #[unsafe(method(outlineView:isItemExpandable:))]
        fn expandable(&self, _o: &NSOutlineView, item: &AnyObject) -> bool {
            let view = self.ivars().view.borrow();
            match (key_row(Some(item)), view.as_ref()) {
                (Some(Row { device, agent: None }), Some(v)) => !v.devices[device].agent_list().is_empty(),
                _ => false,
            }
        }

        #[unsafe(method_id(outlineView:objectValueForTableColumn:byItem:))]
        fn value(&self, _o: &NSOutlineView, column: Option<&NSTableColumn>, item: Option<&AnyObject>) -> Option<Retained<AnyObject>> {
            self.cell_text(column, item).map(|text| Retained::into_super(Retained::into_super(NSString::from_str(&text))))
        }
    }

    unsafe impl NSOutlineViewDelegate for Delegate {
        #[unsafe(method(outlineViewSelectionDidChange:))]
        fn selection_changed(&self, _n: &NSNotification) {
            let view = self.ivars().view.borrow();
            if let Some(v) = view.as_ref() {
                let text = model::hint(v, self.selected(), self.ivars().shown.borrow().len());
                self.set_hint(&text);
            }
        }
    }

    impl Delegate {
        #[unsafe(method(search:))]
        fn search_action(&self, _sender: Option<&AnyObject>) {
            self.apply();
        }

        #[unsafe(method(filter:))]
        fn filter_action(&self, _sender: Option<&AnyObject>) {
            self.apply();
        }

        #[unsafe(method(refresh:))]
        fn refresh_action(&self, _sender: Option<&AnyObject>) {
            self.reload();
        }

        #[unsafe(method(copyRow:))]
        fn copy_row(&self, _sender: Option<&AnyObject>) {
            self.copy_selected();
        }

        /// Edit > Copy reaches us through the window delegate when the list has focus; the
        /// search field handles its own copy before that.
        #[unsafe(method(copy:))]
        fn copy(&self, _sender: Option<&AnyObject>) {
            self.copy_selected();
        }

        #[unsafe(method(focusSearch:))]
        fn focus_search(&self, _sender: Option<&AnyObject>) {
            if let (Some(w), Some(s)) = (self.ivars().window.get(), self.ivars().search.get()) {
                w.makeFirstResponder(Some(s));
            }
        }
    }
);

impl Delegate {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(Ivars::default());
        unsafe { msg_send![super(this), init] }
    }

    fn cell_text(&self, column: Option<&NSTableColumn>, item: Option<&AnyObject>) -> Option<String> {
        let col: usize = column?.identifier().to_string().parse().ok()?;
        let row = key_row(item)?;
        let view = self.ivars().view.borrow();
        Some(model::cells(view.as_ref()?, row).get(col)?.clone())
    }

    fn key(&self, row: Row) -> Retained<NSString> {
        self.ivars()
            .keys
            .borrow_mut()
            .entry(row)
            .or_insert_with(|| match row.agent {
                None => NSString::from_str(&row.device.to_string()),
                Some(a) => NSString::from_str(&format!("{}:{a}", row.device)),
            })
            .clone()
    }

    fn target(&self) -> &AnyObject {
        let obj: &NSObject = self;
        obj
    }

    fn label(&self, text: &str, size: f64, frame: NSRect, mask: NSAutoresizingMaskOptions) -> Retained<NSTextField> {
        let l = NSTextField::labelWithString(&NSString::from_str(text), self.mtm());
        l.setFrame(frame);
        l.setFont(Some(&NSFont::systemFontOfSize(size)));
        l.setAutoresizingMask(mask);
        l
    }

    fn build(&self) {
        let mtm = self.mtm();
        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
        self.menus(&app);

        let style = NSWindowStyleMask::Titled | NSWindowStyleMask::Closable | NSWindowStyleMask::Miniaturizable | NSWindowStyleMask::Resizable;
        let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(W, H));
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(NSWindow::alloc(mtm), rect, style, NSBackingStoreType::Buffered, false)
        };
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(&NSString::from_str(model::TITLE));
        window.setContentMinSize(NSSize::new(640.0, 360.0));
        window.setDelegate(Some(ProtocolObject::from_ref(self)));
        let content = window.contentView().expect("content view");
        let top = NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewMinYMargin;
        let right_top = NSAutoresizingMaskOptions::ViewMinXMargin | NSAutoresizingMaskOptions::ViewMinYMargin;
        let bottom = NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewMaxYMargin;

        let summary = self.label("正在读取...", 13.0, NSRect::new(NSPoint::new(16.0, H - 34.0), NSSize::new(W - 32.0, 20.0)), top);
        content.addSubview(&summary);

        let (seg_w, btn_w, gap) = (330.0, 80.0, 10.0);
        let search_w = W - 32.0 - seg_w - btn_w - 2.0 * gap;
        let search = NSSearchField::initWithFrame(NSSearchField::alloc(mtm), NSRect::new(NSPoint::new(16.0, H - 68.0), NSSize::new(search_w, 26.0)));
        search.setPlaceholderString(Some(&NSString::from_str(model::SEARCH_HINT)));
        search.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewMinYMargin);
        unsafe {
            search.setTarget(Some(self.target()));
            search.setAction(Some(sel!(search:)));
        }
        content.addSubview(&search);

        let labels: Vec<Retained<NSString>> = FILTERS.iter().map(|(t, _)| NSString::from_str(t)).collect();
        let labels = NSArray::from_retained_slice(&labels);
        let filter = unsafe {
            NSSegmentedControl::segmentedControlWithLabels_trackingMode_target_action(
                &labels,
                NSSegmentSwitchTracking::SelectOne,
                Some(self.target()),
                Some(sel!(filter:)),
                mtm,
            )
        };
        filter.setSelectedSegment(0);
        filter.setFrame(NSRect::new(NSPoint::new(16.0 + search_w + gap, H - 68.0), NSSize::new(seg_w, 26.0)));
        filter.setAutoresizingMask(right_top);
        content.addSubview(&filter);

        let refresh = unsafe { NSButton::buttonWithTitle_target_action(ns_string!("刷新"), Some(self.target()), Some(sel!(refresh:)), mtm) };
        refresh.setFrame(NSRect::new(NSPoint::new(W - 16.0 - btn_w, H - 69.0), NSSize::new(btn_w, 28.0)));
        refresh.setAutoresizingMask(right_top);
        content.addSubview(&refresh);

        let list_rect = NSRect::new(NSPoint::new(16.0, 52.0), NSSize::new(W - 32.0, H - 52.0 - 80.0));
        let scroll = NSScrollView::initWithFrame(NSScrollView::alloc(mtm), list_rect);
        scroll.setHasVerticalScroller(true);
        scroll.setHasHorizontalScroller(true);
        scroll.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable);
        let outline = NSOutlineView::initWithFrame(NSOutlineView::alloc(mtm), NSRect::new(NSPoint::new(0.0, 0.0), list_rect.size));
        let widths = [230.0, 130.0, 250.0, 120.0, 210.0];
        for (i, title) in COLUMNS.iter().enumerate() {
            let col = NSTableColumn::initWithIdentifier(NSTableColumn::alloc(mtm), &NSString::from_str(&i.to_string()));
            col.setTitle(&NSString::from_str(title));
            col.setWidth(widths[i]);
            col.setEditable(false);
            outline.addTableColumn(&col);
            if i == 0 {
                unsafe { outline.setOutlineTableColumn(Some(&col)) };
            }
        }
        outline.setUsesAlternatingRowBackgroundColors(true);
        unsafe {
            outline.setDataSource(Some(ProtocolObject::from_ref(self)));
            outline.setDelegate(Some(ProtocolObject::from_ref(self)));
            outline.setTarget(Some(self.target()));
            outline.setDoubleAction(Some(sel!(copyRow:)));
        }
        scroll.setDocumentView(Some(&outline));
        content.addSubview(&scroll);

        let hint = self.label("", 13.0, NSRect::new(NSPoint::new(16.0, 26.0), NSSize::new(W - 32.0, 18.0)), bottom);
        content.addSubview(&hint);
        let note = self.label(model::NOTE, 11.0, NSRect::new(NSPoint::new(16.0, 8.0), NSSize::new(W - 32.0, 16.0)), bottom);
        note.setTextColor(Some(&NSColor::secondaryLabelColor()));
        content.addSubview(&note);

        window.center();
        window.makeKeyAndOrderFront(None);
        window.makeFirstResponder(Some(&search));
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);

        let iv = self.ivars();
        let _ = iv.window.set(window);
        let _ = iv.outline.set(outline);
        let _ = iv.search.set(search);
        let _ = iv.filter.set(filter);
        let _ = iv.refresh.set(refresh);
        let _ = iv.summary.set(summary);
        let _ = iv.hint.set(hint);
    }

    fn menus(&self, app: &NSApplication) {
        let mtm = self.mtm();
        let bar = NSMenu::new(mtm);
        let item = |title: &str, action: Sel, key: &str, target: bool| {
            let i = unsafe {
                NSMenuItem::initWithTitle_action_keyEquivalent(NSMenuItem::alloc(mtm), &NSString::from_str(title), Some(action), &NSString::from_str(key))
            };
            if target {
                unsafe { i.setTarget(Some(self.target())) };
            }
            i
        };
        let submenu = |title: &str, items: Vec<Retained<NSMenuItem>>| {
            let m = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str(title));
            for i in &items {
                m.addItem(i);
            }
            let top = NSMenuItem::new(mtm);
            top.setSubmenu(Some(&m));
            bar.addItem(&top);
        };
        submenu("fMesh", vec![item("退出 fMesh", sel!(terminate:), "q", false)]);
        submenu(
            "编辑",
            vec![
                item("拷贝", sel!(copy:), "c", false),
                item("粘贴", sel!(paste:), "v", false),
                item("全选", sel!(selectAll:), "a", false),
                item("查找", sel!(focusSearch:), "f", true),
            ],
        );
        submenu("显示", vec![item("刷新", sel!(refresh:), "r", true), item("关闭窗口", sel!(performClose:), "w", false)]);
        app.setMainMenu(Some(&bar));
    }

    fn set_hint(&self, text: &str) {
        if let Some(h) = self.ivars().hint.get() {
            h.setStringValue(&NSString::from_str(text));
        }
    }

    fn selected(&self) -> Option<Row> {
        let o = self.ivars().outline.get()?;
        let r = o.selectedRow();
        if r < 0 {
            return None;
        }
        key_row(o.itemAtRow(r).as_deref())
    }

    fn copy_selected(&self) {
        let view = self.ivars().view.borrow();
        let (Some(v), Some(row)) = (view.as_ref(), self.selected()) else {
            self.set_hint("先选中一台设备或一个 Agent, 再复制");
            return;
        };
        let text = model::copy_text(v, row);
        let pb = NSPasteboard::generalPasteboard();
        pb.clearContents();
        let ok = pb.setString_forType(&NSString::from_str(&text), unsafe { NSPasteboardTypeString });
        self.set_hint(&if ok { model::copied(v, row) } else { "复制失败, 请重试".into() });
    }

    /// Starts a background load; the list stays usable meanwhile.
    fn reload(&self) {
        if self.ivars().loading.replace(true) {
            return;
        }
        self.set_hint("正在查询设备与 Agent 目录...");
        if let Some(b) = self.ivars().refresh.get() {
            b.setEnabled(false);
        }
        std::thread::spawn(|| {
            let view = crate::load();
            dispatch2::DispatchQueue::main().exec_async(move || {
                APP.with(|app| {
                    if let Some(d) = app.get() {
                        d.loaded(view);
                    }
                })
            });
        });
    }

    fn loaded(&self, view: View) {
        let iv = self.ivars();
        iv.loading.set(false);
        if let Some(b) = iv.refresh.get() {
            b.setEnabled(true);
        }
        if let Some(s) = iv.summary.get() {
            s.setStringValue(&NSString::from_str(&model::summary(&view)));
        }
        iv.keys.borrow_mut().clear();
        *iv.view.borrow_mut() = Some(view);
        self.apply();
        let n = iv.shown.borrow().len();
        let total = iv.view.borrow().as_ref().map_or(0, |v| v.devices.len());
        self.set_hint(&format!("已更新 · 显示 {n} / {total} 台"));
    }

    fn apply(&self) {
        let iv = self.ivars();
        let query = iv.search.get().map(|s| s.stringValue().to_string()).unwrap_or_default();
        let filter = iv.filter.get().map_or(0, |f| f.selectedSegment().max(0) as usize);
        let filter = FILTERS[filter.min(FILTERS.len() - 1)].1;
        let expand = {
            let view = iv.view.borrow();
            let Some(v) = view.as_ref() else { return };
            *iv.shown.borrow_mut() = model::visible(v, &query, filter);
            model::expand(v, &query)
        };
        let Some(o) = iv.outline.get() else { return };
        o.reloadData();
        for d in expand {
            let key = self.key(Row { device: d, agent: None });
            unsafe { o.expandItem(Some(&key)) };
        }
        let view = iv.view.borrow();
        if let Some(v) = view.as_ref() {
            self.set_hint(&model::hint(v, None, iv.shown.borrow().len()));
        }
    }
}

pub fn run() {
    let mtm = MainThreadMarker::new().expect("main thread");
    let app = NSApplication::sharedApplication(mtm);
    let delegate = Delegate::new(mtm);
    app.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    APP.with(|a| {
        let _ = a.set(delegate);
    });
    app.run();
}
