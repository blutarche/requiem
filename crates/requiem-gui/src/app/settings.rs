//! Settings: each account, its state, and what can be done about it there.

use std::cell::RefCell;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject, Sel};
use objc2::{MainThreadOnly, Message, msg_send, sel};
use objc2_app_kit::{
    NSAccessibility, NSAccessibilityGroupRole, NSBorderType, NSControl, NSFocusRingType,
    NSLineBreakMode, NSScrollView, NSTextField, NSTextFieldDelegate, NSView,
};
use objc2_core_foundation::CGFloat;
use objc2_foundation::{MainThreadMarker, NSRect, NSSize};
use requiem_core::ProviderId;

use super::Delegate;
use super::badge::Badge;
use super::shelves::FlippedView;
use super::theme::{self, Icon, Weight};
use super::ui;
use super::widgets::{
    self, FLEX_H, FLEX_W, Field, HoverButton, LABEL_ICON, Mask, PIN_LEFT, PIN_RIGHT, PIN_TOP,
    SecretField, rect,
};
use crate::vm::{self, AccountRow, AccountRowState, Service};

const PAD: CGFloat = 24.0;
/// The window has a transparent title bar over the content; the header starts below it.
const TOP: CGFloat = 20.0;
/// As tall as the Library's header, so the titles line up.
const HEAD_H: CGFloat = Field::H + 8.0;
/// An account row's line: badge, name, status and buttons.
const ROW_H: CGFloat = 64.0;
const BADGE: CGFloat = 20.0;
/// Where the name, and the code or form below it, start.
const TEXT_X: CGFloat = PAD + BADGE + 12.0;
const ACTION_H: CGFloat = 32.0;
const BUTTON_H: CGFloat = 36.0;
const GAP: CGFloat = 8.0;
const FIELD_W: CGFloat = 440.0;
/// The wrapped notes' widest line.
const NOTE_W: CGFloat = 560.0;
/// A field's focus ring reaches this far past its box.
const RING: CGFloat = 2.0;

/// A view whose subviews lay out top down.
fn flipped(mtm: MainThreadMarker, mask: Mask) -> Retained<FlippedView> {
    // SAFETY: `initWithFrame:` is NSView's designated initializer.
    let v: Retained<FlippedView> =
        unsafe { msg_send![FlippedView::alloc(mtm), initWithFrame: NSRect::ZERO] };
    v.setAutoresizingMask(mask);
    v.setWantsLayer(true);
    v
}

/// A label that wraps to as many lines as its text needs at its width.
fn wrapping(mtm: MainThreadMarker, font_size: CGFloat) -> Retained<NSTextField> {
    let l = widgets::label(
        mtm,
        "",
        &theme::font(font_size, Weight::Regular),
        &theme::muted(),
    );
    l.setUsesSingleLineMode(false);
    l.setMaximumNumberOfLines(0);
    l.setLineBreakMode(NSLineBreakMode::ByWordWrapping);
    l.setAutoresizingMask(PIN_LEFT);
    l
}

/// Places the wrapping label `l` at `x`, `y`, `width` wide and as tall as its text; returns its
/// bottom.
fn wrap_at(l: &NSTextField, x: CGFloat, y: CGFloat, width: CGFloat) -> CGFloat {
    let h = l.cell().map_or(0.0, |c| {
        c.cellSizeForBounds(rect(0.0, 0.0, width, 10_000.0))
            .height
            .ceil()
    });
    l.setFrame(rect(x, y, width, h));
    y + h
}

/// A one-line label, `y` down from the top of a flipped view.
fn line_at(l: &NSTextField, x: CGFloat, y: CGFloat, width: CGFloat) -> CGFloat {
    let h = l.font().map_or(0.0, |f| widgets::line_height(&f));
    l.setFrame(rect(x, y, width, h));
    y + h
}

fn button(
    mtm: MainThreadMarker,
    primary: bool,
    title: &str,
    icon: Option<Icon>,
    target: &AnyObject,
    action: Sel,
    tag: isize,
) -> Retained<HoverButton> {
    let frame = rect(0.0, 0.0, 0.0, BUTTON_H);
    let b = if primary {
        HoverButton::primary(mtm, frame)
    } else {
        HoverButton::secondary(mtm, frame)
    };
    relabel(&b, title, icon, BUTTON_H);
    b.set_action(target, action);
    b.setTag(tag);
    b.setAutoresizingMask(PIN_LEFT);
    b
}

fn relabel(b: &HoverButton, title: &str, icon: Option<Icon>, h: CGFloat) {
    let image = icon.and_then(|i| i.image(LABEL_ICON));
    let w = b.set_label_image(title, image.as_deref());
    b.setFrameSize(NSSize::new(w, h));
}

/// Lays the shown `buttons` out from `x`, `y` down, starting a new line before one that would
/// pass `right`; returns the bottom.
fn flow(buttons: &[&HoverButton], x: CGFloat, y: CGFloat, right: CGFloat) -> CGFloat {
    let (mut bx, mut by) = (x, y);
    for b in buttons.iter().filter(|b| !b.isHidden()) {
        let w = b.frame().size.width;
        if bx > x && bx + w > right {
            bx = x;
            by += BUTTON_H + GAP;
        }
        b.setFrame(rect(bx, by, w, BUTTON_H));
        bx += w + GAP;
    }
    by + BUTTON_H
}

pub struct SettingsPage {
    pub view: Retained<NSView>,
    scroll: Retained<NSScrollView>,
    doc: Retained<FlippedView>,
    heading: Retained<NSTextField>,
    /// Closes the last row.
    end: Retained<NSView>,
    rows: RefCell<Vec<AccountView>>,
    /// What the rows show; their buttons' tags index it.
    shown: RefCell<Vec<AccountRow>>,
    /// The buttons' target and the fields' delegate.
    target: Retained<Delegate>,
}

impl SettingsPage {
    pub fn new(mtm: MainThreadMarker, frame: NSRect, target: &Delegate) -> SettingsPage {
        let (w, h) = (frame.size.width, frame.size.height);
        let view = widgets::view(mtm, frame, FLEX_W | FLEX_H);
        let head = widgets::view(
            mtm,
            rect(0.0, h - TOP - HEAD_H, w, HEAD_H),
            FLEX_W | PIN_TOP,
        );
        let title = widgets::placed_label(
            mtm,
            "Settings",
            &theme::font(20.0, Weight::SemiBold),
            &theme::ink(),
            PAD,
            HEAD_H / 2.0,
            200.0,
            PIN_TOP | PIN_LEFT,
        );
        head.addSubview(&title);
        // Below the header, so it never reaches under the title bar: see Home's shelves.
        let scroll = NSScrollView::initWithFrame(
            NSScrollView::alloc(mtm),
            rect(0.0, 0.0, w, h - TOP - HEAD_H - GAP),
        );
        scroll.setAutoresizingMask(FLEX_W | FLEX_H);
        scroll.setHasVerticalScroller(true);
        scroll.setAutohidesScrollers(true);
        scroll.setDrawsBackground(false);
        scroll.setBorderType(NSBorderType::NoBorder);
        scroll.contentView().setDrawsBackground(false);
        scroll.setFocusRingType(NSFocusRingType::None);
        let doc = flipped(mtm, FLEX_W);
        doc.setFrame(rect(0.0, 0.0, w, 0.0));
        scroll.setDocumentView(Some(&doc));
        let heading = widgets::label(
            mtm,
            "Accounts",
            &theme::font(17.0, Weight::SemiBold),
            &theme::ink(),
        );
        let end = widgets::panel(mtm, NSRect::ZERO, FLEX_W, &theme::line());
        doc.addSubview(&heading);
        doc.addSubview(&end);
        view.addSubview(&head);
        view.addSubview(&scroll);
        view.setHidden(true);
        SettingsPage {
            view,
            scroll,
            doc,
            heading,
            end,
            rows: RefCell::new(Vec::new()),
            shown: RefCell::new(Vec::new()),
            target: target.retain(),
        }
    }

    /// Shows `rows`. With `speak`, a row whose status changed says so, as does a newly
    /// refused client. True while a row shows a sign-in code, whose expiry needs redrawing.
    pub fn set(&self, rows: Vec<AccountRow>, speak: bool) -> bool {
        let mtm = self.view.mtm();
        {
            let mut views = self.rows.borrow_mut();
            while views.len() < rows.len() {
                let v = AccountView::new(mtm, views.len(), &self.target);
                self.doc.addSubview(&v.view);
                views.push(v);
            }
            for (i, v) in views.iter().enumerate() {
                match rows.get(i) {
                    Some(row) => v.show(row, speak),
                    None => v.hide(),
                }
            }
        }
        let pending = rows
            .iter()
            .any(|r| matches!(r.state, AccountRowState::Pending { .. }));
        *self.shown.borrow_mut() = rows;
        self.layout();
        pending
    }

    /// Stacks the rows at the page's width, which their text and buttons wrap to.
    pub fn layout(&self) {
        let w = self.scroll.contentSize().width;
        let mut y = GAP;
        y = line_at(&self.heading, PAD, y, w - 2.0 * PAD) + GAP;
        let views = self.rows.borrow();
        let shown = self.shown.borrow();
        for (v, row) in views.iter().zip(shown.iter()) {
            y += v.place(row, y, w);
        }
        self.end.setFrame(rect(PAD, y, w - 2.0 * PAD, 1.0));
        self.end.setHidden(shown.is_empty());
        self.doc.setFrame(rect(0.0, 0.0, w, y + PAD));
    }

    /// Row `i` as shown.
    pub fn row(&self, i: usize) -> Option<AccountRow> {
        self.shown.borrow().get(i).cloned()
    }

    /// Row `i`'s provider, and what its client form holds while it shows one.
    pub fn client(&self, i: usize) -> Option<(ProviderId, String, String)> {
        let row = self.row(i)?;
        if !matches!(row.state, AccountRowState::Setup(_)) {
            return None;
        }
        let views = self.rows.borrow();
        let form = &views.get(i)?.form;
        Some((row.provider, form.id.value(), form.secret.value()))
    }

    pub fn clear_secret(&self, i: usize) {
        if let Some(v) = self.rows.borrow().get(i) {
            v.form.secret.clear();
            v.form.update_save();
        }
    }

    /// The row whose client form `control` is a field of.
    pub fn form_row(&self, control: &NSControl) -> Option<usize> {
        self.rows
            .borrow()
            .iter()
            .position(|v| v.form.id.is(control) || v.form.secret.is(control))
    }

    /// Enables Save once both of row `i`'s fields hold something.
    pub fn fields_changed(&self, i: usize) {
        if let Some(v) = self.rows.borrow().get(i) {
            v.form.update_save();
        }
    }

    /// Puts the cursor in the first shown client form's Client ID field.
    pub fn focus_form(&self) {
        let field = self
            .rows
            .borrow()
            .iter()
            .find(|v| !v.view.isHidden() && !v.form.view.isHidden())
            .map(|v| v.form.id.text.retain());
        let Some(field) = field else {
            return;
        };
        if let Some(window) = self.view.window() {
            window.makeFirstResponder(Some(&field));
        }
        field.scrollRectToVisible(field.bounds());
    }

    /// Redraws the sign-in codes' expiry; true while any shows.
    pub fn refresh_expiry(&self, now_unix: u64) -> bool {
        let views = self.rows.borrow();
        let shown = self.shown.borrow();
        let mut any = false;
        for (v, row) in views.iter().zip(shown.iter()) {
            if let AccountRowState::Pending {
                expires_at_unix, ..
            } = row.state
            {
                widgets::set_text(&v.code.expiry, &vm::expires_text(expires_at_unix, now_unix));
                any = true;
            }
        }
        any
    }
}

/// An account's row: the service's badge, name and state, its buttons, and below them the
/// sign-in code or the client form.
struct AccountView {
    /// A VoiceOver group named for the service and its state.
    view: Retained<FlippedView>,
    rule: Retained<NSView>,
    badge: Retained<Badge>,
    name: Retained<NSTextField>,
    status: Retained<NSTextField>,
    /// Sign In… or Sign Out….
    action: Retained<HoverButton>,
    change: Retained<HoverButton>,
    code: CodeView,
    form: FormView,
    /// The status last shown, which a change is announced against.
    said: RefCell<String>,
}

impl AccountView {
    /// Row `i`; its buttons are tagged with `i`.
    fn new(mtm: MainThreadMarker, i: usize, target: &Delegate) -> AccountView {
        let tag = i as isize;
        let delegate = ProtocolObject::<dyn NSTextFieldDelegate>::from_ref(target);
        let target: &AnyObject = target.as_ref();
        let view = flipped(mtm, FLEX_W);
        view.setAccessibilityElement(true);
        // SAFETY: the role static is an immutable framework constant.
        view.setAccessibilityRole(Some(unsafe { NSAccessibilityGroupRole }));
        let rule = widgets::panel(mtm, NSRect::ZERO, FLEX_W, &theme::line());
        let badge = Badge::new(mtm, rect(PAD, (ROW_H - BADGE) / 2.0, BADGE, BADGE));
        let name = widgets::label(mtm, "", &theme::font(14.0, Weight::Medium), &theme::ink());
        let status = widgets::label(
            mtm,
            "",
            &theme::font(12.0, Weight::Regular),
            &theme::muted(),
        );
        // The group's name says both.
        widgets::hide_from_accessibility(&name);
        widgets::hide_from_accessibility(&status);
        let action = button(mtm, false, "", None, target, sel!(settingsSignIn:), tag);
        let change = button(
            mtm,
            false,
            "Change Google Client\u{2026}",
            Some(Icon::KeyRound),
            target,
            sel!(changeClient:),
            tag,
        );
        for b in [&action, &change] {
            b.setAutoresizingMask(PIN_RIGHT);
            b.setFrameSize(NSSize::new(b.frame().size.width, ACTION_H));
        }
        let code = CodeView::new(mtm, target, tag);
        let form = FormView::new(mtm, target, delegate, tag);
        for v in [&rule, &badge as &NSView, &name, &status, &action, &change] {
            view.addSubview(v);
        }
        view.addSubview(&code.view);
        view.addSubview(&form.view);
        AccountView {
            view,
            rule,
            badge,
            name,
            status,
            action,
            change,
            code,
            form,
            said: RefCell::new(String::new()),
        }
    }

    fn show(&self, row: &AccountRow, speak: bool) {
        let mtm = self.view.mtm();
        let name = vm::provider_name(&row.provider.0);
        self.view.setHidden(false);
        self.badge.set(Service::of(&row.provider));
        widgets::set_text(&self.name, name);
        widgets::set_text(&self.status, &row.status);
        widgets::accessibility_label(&self.view, &format!("{name}, {}", row.status));
        let was = self.said.replace(row.status.clone());
        if speak && !was.is_empty() && was != row.status {
            widgets::announce(mtm, &format!("{name}, {}", row.status));
        }
        let action = match &row.state {
            AccountRowState::SignedIn { .. } => {
                Some(("Sign Out\u{2026}", Icon::LogOut, sel!(settingsSignOut:)))
            }
            AccountRowState::SignedOut => {
                Some(("Sign In\u{2026}", Icon::LogIn, sel!(settingsSignIn:)))
            }
            _ => None,
        };
        if let Some((title, icon, sel)) = action {
            relabel(&self.action, title, Some(icon), ACTION_H);
            // SAFETY: the target `new` gave the button implements both actions.
            unsafe { self.action.setAction(Some(sel)) };
        }
        self.action.set_shown(action.is_some());
        self.change.set_shown(row.change_client);
        match &row.state {
            AccountRowState::Pending {
                verification_url,
                user_code,
                expires_at_unix,
            } => self
                .code
                .show(&row.provider, verification_url, user_code, *expires_at_unix),
            _ => self.code.view.setHidden(true),
        }
        match &row.state {
            AccountRowState::Setup(form) => self.form.show(form, speak),
            _ => self.form.hide(),
        }
    }

    fn hide(&self) {
        self.form.hide();
        self.view.setHidden(true);
        self.said.borrow_mut().clear();
    }

    /// Lays the row out `y` down the page, `w` wide; returns its height.
    fn place(&self, row: &AccountRow, y: CGFloat, w: CGFloat) -> CGFloat {
        // Sized first: a later change of width would move the subviews placed for this one.
        self.view.setFrame(rect(0.0, y, w, ROW_H));
        self.rule.setFrame(rect(PAD, 0.0, w - 2.0 * PAD, 1.0));
        let mut right = w - PAD;
        for b in [&self.action, &self.change] {
            if b.isHidden() {
                continue;
            }
            let bw = b.frame().size.width;
            right -= bw;
            b.setFrame(rect(right, (ROW_H - ACTION_H) / 2.0, bw, ACTION_H));
            right -= GAP;
        }
        let text_w = (right - 4.0 - TEXT_X).max(0.0);
        let name_h = widgets::line_height(&theme::font(14.0, Weight::Medium));
        let status_h = widgets::line_height(&theme::font(12.0, Weight::Regular));
        let top = ((ROW_H - name_h - status_h) / 2.0).round();
        line_at(&self.name, TEXT_X, top, text_w);
        line_at(&self.status, TEXT_X, top + name_h, text_w);
        let detail_w = w - TEXT_X - PAD;
        let detail = match row.state {
            AccountRowState::Pending { .. } => {
                let v = &self.code.view;
                v.setFrame(rect(TEXT_X, ROW_H, detail_w, 0.0));
                let h = self.code.place(detail_w);
                v.setFrameSize(NSSize::new(detail_w, h));
                h
            }
            AccountRowState::Setup(_) => {
                // The fields' focus rings reach past the form's left edge.
                let (v, form_w) = (&self.form.view, detail_w + RING);
                v.setFrame(rect(TEXT_X - RING, ROW_H, form_w, 0.0));
                let h = self.form.place(form_w);
                v.setFrameSize(NSSize::new(form_w, h));
                h
            }
            _ => 0.0,
        };
        let h = if detail > 0.0 {
            ROW_H + detail + PAD
        } else {
            ROW_H
        };
        self.view.setFrameSize(NSSize::new(w, h));
        h
    }
}

/// A pending sign-in's code, as the Library's sign-in panel shows it.
struct CodeView {
    view: Retained<FlippedView>,
    hint: Retained<NSTextField>,
    code: Retained<NSTextField>,
    expiry: Retained<NSTextField>,
    /// What the service asks the user to know before signing in; empty for most.
    notice: Retained<NSTextField>,
    copy: Retained<HoverButton>,
    open: Retained<HoverButton>,
    cancel: Retained<HoverButton>,
}

impl CodeView {
    fn new(mtm: MainThreadMarker, target: &AnyObject, tag: isize) -> CodeView {
        let view = flipped(mtm, FLEX_W);
        let hint = wrapping(mtm, 14.0);
        let code = widgets::label(mtm, "", &theme::font(32.0, Weight::SemiBold), &theme::ink());
        let expiry = widgets::label(
            mtm,
            "",
            &theme::font(12.0, Weight::Regular),
            &theme::muted(),
        );
        let notice = wrapping(mtm, 12.0);
        for l in [&hint, &code] {
            l.setSelectable(true);
        }
        let copy = button(
            mtm,
            true,
            "Copy Code",
            Some(Icon::Copy),
            target,
            sel!(settingsCopyCode:),
            tag,
        );
        let open = button(
            mtm,
            false,
            "Open in Browser",
            Some(Icon::ExternalLink),
            target,
            sel!(settingsOpenHost:),
            tag,
        );
        let cancel = button(
            mtm,
            false,
            "Cancel",
            None,
            target,
            sel!(settingsCancelSignIn:),
            tag,
        );
        for v in [&hint, &code, &expiry, &notice] {
            view.addSubview(v);
        }
        for b in [&copy, &open, &cancel] {
            view.addSubview(b);
        }
        view.setHidden(true);
        CodeView {
            view,
            hint,
            code,
            expiry,
            notice,
            copy,
            open,
            cancel,
        }
    }

    fn show(&self, provider: &ProviderId, url: &str, code: &str, expires_at_unix: u64) {
        widgets::set_text(&self.hint, &vm::sign_in_hint(url));
        widgets::set_text(&self.code, code);
        widgets::set_text(
            &self.expiry,
            &vm::expires_text(expires_at_unix, ui::unix_now()),
        );
        let notice = vm::sign_in_notice(provider).unwrap_or("");
        widgets::set_text(&self.notice, notice);
        self.notice.setHidden(notice.is_empty());
        self.view.setHidden(false);
    }

    /// Lays the code out `w` wide; returns its height.
    fn place(&self, w: CGFloat) -> CGFloat {
        let note_w = w.min(NOTE_W);
        let mut y = wrap_at(&self.hint, 0.0, 0.0, note_w);
        y = line_at(&self.code, 0.0, y + 4.0, w);
        y = line_at(&self.expiry, 0.0, y, w);
        if !self.notice.isHidden() {
            y = wrap_at(&self.notice, 0.0, y + GAP, note_w);
        }
        flow(&[&self.copy, &self.open, &self.cancel], 0.0, y + 16.0, w)
    }
}

/// The form for the user's own Google client: its ID and secret, or its file.
struct FormView {
    view: Retained<FlippedView>,
    /// Why the form shows, or in red why the last client was refused.
    note: Retained<NSTextField>,
    id_label: Retained<NSTextField>,
    id: Field,
    secret_label: Retained<NSTextField>,
    secret: SecretField,
    save: Retained<HoverButton>,
    choose: Retained<HoverButton>,
    guide: Retained<HoverButton>,
    cancel: Retained<HoverButton>,
}

impl FormView {
    fn new(
        mtm: MainThreadMarker,
        target: &AnyObject,
        delegate: &ProtocolObject<dyn NSTextFieldDelegate>,
        tag: isize,
    ) -> FormView {
        let view = flipped(mtm, FLEX_W);
        let note = wrapping(mtm, 12.0);
        note.setSelectable(true);
        let field_label = |text: &str| {
            let l = widgets::label(
                mtm,
                text,
                &theme::font(12.0, Weight::Medium),
                &theme::ink_2(),
            );
            // The field it names carries the name.
            widgets::hide_from_accessibility(&l);
            l
        };
        let id_label = field_label("Client ID");
        let secret_label = field_label("Client secret");
        let field = rect(RING, 0.0, FIELD_W, Field::H);
        let id = Field::new(
            mtm,
            field,
            PIN_LEFT,
            "123-abc.apps.googleusercontent.com",
            None,
        );
        let secret = SecretField::new(mtm, field, PIN_LEFT, "GOCSPX-\u{2026}");
        // SAFETY: the fields' delegate is weak. It is the app delegate, which `app::run` holds
        // in a local until `app.run()`, which never returns.
        unsafe {
            id.text.setDelegate(Some(delegate));
            secret.text.setDelegate(Some(delegate));
        }
        widgets::accessibility_label(&id.text, "Client ID");
        widgets::accessibility_label(&secret.text, "Client secret");
        let save = button(mtm, true, "Save", None, target, sel!(saveClient:), tag);
        let choose = button(
            mtm,
            false,
            "Choose Client File\u{2026}",
            Some(Icon::FolderOpen),
            target,
            sel!(settingsChooseClientFile:),
            tag,
        );
        let guide = button(
            mtm,
            false,
            "Setup Guide",
            Some(Icon::BookOpen),
            target,
            sel!(openSetupGuide:),
            tag,
        );
        let cancel = button(mtm, false, "Cancel", None, target, sel!(cancelSetup:), tag);
        for v in [&note, &id_label, &secret_label] {
            view.addSubview(v);
        }
        view.addSubview(&id.ring);
        view.addSubview(&secret.ring);
        for b in [&save, &choose, &guide, &cancel] {
            view.addSubview(b);
        }
        view.setHidden(true);
        let form = FormView {
            view,
            note,
            id_label,
            id,
            secret_label,
            secret,
            save,
            choose,
            guide,
            cancel,
        };
        form.update_save();
        form
    }

    fn show(&self, form: &vm::ClientForm, speak: bool) {
        match &form.error {
            Some(e) => {
                if speak && self.note.stringValue().to_string() != *e {
                    widgets::announce(self.view.mtm(), e);
                }
                widgets::set_text(&self.note, e);
                self.note.setTextColor(Some(&theme::bad()));
            }
            None => {
                widgets::set_text(&self.note, &form.reason);
                self.note.setTextColor(Some(&theme::muted()));
            }
        }
        self.cancel.set_shown(form.cancellable);
        self.view.setHidden(false);
        self.update_save();
    }

    /// Collapses the form, emptied, so the next one starts blank.
    fn hide(&self) {
        if self.view.isHidden() {
            return;
        }
        let editing =
            self.id.text.currentEditor().is_some() || self.secret.text.currentEditor().is_some();
        if editing && let Some(window) = self.view.window() {
            window.makeFirstResponder(None);
        }
        self.id.set_value("");
        self.secret.clear();
        self.view.setHidden(true);
    }

    fn update_save(&self) {
        self.save
            .set_enabled(vm::client_filled(&self.id.value(), &self.secret.value()));
    }

    /// Lays the form out `w` wide, its fields' rings `RING` in; returns its height.
    fn place(&self, w: CGFloat) -> CGFloat {
        let x = RING;
        let field_w = (w - x).clamp(0.0, FIELD_W);
        let mut y = wrap_at(&self.note, x, 0.0, (w - x).min(NOTE_W));
        y = line_at(&self.id_label, x, y + 12.0, field_w) + 6.0;
        self.id.ring.setFrame(rect(
            0.0,
            y - RING,
            field_w + 2.0 * RING,
            Field::H + 2.0 * RING,
        ));
        y = line_at(&self.secret_label, x, y + Field::H + 12.0, field_w) + 6.0;
        self.secret.ring.setFrame(rect(
            0.0,
            y - RING,
            field_w + 2.0 * RING,
            Field::H + 2.0 * RING,
        ));
        let buttons = [&self.save, &self.choose, &self.guide, &self.cancel];
        flow(&buttons.map(|b| &**b), x, y + Field::H + 16.0, w)
    }
}
