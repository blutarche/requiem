//! A recycling grid of collection cards: the Library, and search results of one kind.

use std::cell::Cell;
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{gio, glib};
use requiem_core::{Collection, Item};

use super::shelves::{CARD_W, Card};
use super::widgets::{self, attach, attached};
use crate::vm::{self, Place};

const PAD: i32 = 24;
const GAP: i32 = 16;
/// A card's hover fill reaches this far past its content, on every side.
const INSET: i32 = 6;

/// The columns that fill a grid `width` wide, and the side of their square covers: as many
/// as fit cards of Home's size, which then grow to fill the width. As on macOS.
fn layout(width: i32) -> (u32, i32) {
    let room = (width - 2 * PAD).max(CARD_W / 2);
    let cols = ((room + GAP) / (CARD_W + GAP)).max(1);
    let side = (room - (cols - 1) * GAP) / cols;
    (cols as u32, side)
}

#[derive(Clone)]
struct Entry {
    collection: Collection,
    mixed: bool,
    artists: bool,
}

pub struct CoverGrid {
    pub root: gtk::ScrolledWindow,
    pub grid: gtk::GridView,
    store: gio::ListStore,
    selection: gtk::SingleSelection,
    artists: Rc<Cell<bool>>,
    from: Place,
}

impl CoverGrid {
    /// Cards that open from `from`.
    pub fn new(from: Place) -> CoverGrid {
        let store = gio::ListStore::new::<glib::BoxedAnyObject>();
        let selection = gtk::SingleSelection::new(Some(store.clone()));
        selection.set_autoselect(false);
        selection.set_can_unselect(true);
        let factory = gtk::SignalListItemFactory::new();
        let side = Rc::new(Cell::new(CARD_W));
        let store_ = store.downgrade();
        factory.connect_setup(move |_, item| {
            let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            let holder = gtk::Box::new(gtk::Orientation::Vertical, 0);
            holder.set_halign(gtk::Align::Center);
            item.set_child(Some(&holder));
            let (weak, store) = (item.downgrade(), store_.clone());
            widgets::on_click(&holder, move |_| {
                if let (Some(item), Some(store)) = (weak.upgrade(), store.upgrade()) {
                    open(&store, item.position(), from);
                }
            });
        });
        let side_ = side.clone();
        factory.connect_bind(move |_, item| {
            let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            let (Some(holder), Some(obj)) = (
                item.child().and_downcast::<gtk::Box>(),
                item.item().and_downcast::<glib::BoxedAnyObject>(),
            ) else {
                return;
            };
            let entry = obj.borrow::<Entry>();
            let side = side_.get();
            // Same collection as bound before, at the same size: keep its card.
            if attached::<String>(&holder, "id").is_some_and(|id| *id == entry.collection.id.id)
                && attached::<i32>(&holder, "side").is_some_and(|s| *s == side)
                && holder.first_child().is_some()
            {
                return;
            }
            while let Some(c) = holder.first_child() {
                holder.remove(&c);
            }
            let item_ = Item::Collection(entry.collection.clone());
            let badge = vm::badge(&entry.collection.id.provider, entry.mixed);
            let card = Card::sized(&item_, entry.artists, badge, side);
            card.root.remove_css_class("card");
            holder.append(&card.root);
            attach(&holder, "id", entry.collection.id.id.clone());
            attach(&holder, "side", side);
            attach(&holder, "cover", card.cover);
        });
        factory.connect_unbind(|_, item| {
            let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            if let Some(holder) = item.child().and_downcast::<gtk::Box>() {
                while let Some(c) = holder.first_child() {
                    holder.remove(&c);
                }
                attach(&holder, "id", String::new());
            }
        });
        let grid = gtk::GridView::new(Some(selection.clone()), Some(factory));
        grid.add_css_class("cards");
        // Columns from the cards' minimum width; their natural one is their labels' full text.
        grid.set_hscroll_policy(gtk::ScrollablePolicy::Minimum);
        grid.set_max_columns(24);
        grid.set_min_columns(1);
        grid.set_margin_start(PAD - INSET);
        grid.set_margin_end(PAD - INSET);
        let store_ = store.clone();
        grid.connect_activate(move |_, pos| {
            open(&store_, pos, from);
        });
        let root = widgets::vscroll(&grid);
        // The grid's width, which its columns and their cards' size follow.
        let (g, st, sd) = (grid.downgrade(), store.downgrade(), side.clone());
        root.hadjustment().connect_changed(move |adj| {
            let (Some(grid), Some(store)) = (g.upgrade(), st.upgrade()) else {
                return;
            };
            let width = adj.page_size() as i32 + 2 * (PAD - INSET);
            if width <= 2 * PAD {
                return;
            }
            let (cols, side) = layout(width);
            // Not the minimum too: that would make the grid ask for its current width,
            // and the window could then only grow.
            grid.set_max_columns(cols);
            if sd.replace(side) != side {
                // New objects for the same entries, so every card is bound again at its new
                // size; after this allocation.
                super::theme::later(move || {
                    let items: Vec<glib::Object> = (0..store.n_items())
                        .filter_map(|i| store.item(i).and_downcast::<glib::BoxedAnyObject>())
                        .map(|o| glib::BoxedAnyObject::new(o.borrow::<Entry>().clone()).upcast())
                        .collect();
                    store.splice(0, store.n_items(), &items);
                });
            }
        });
        CoverGrid {
            root,
            grid,
            store,
            selection,
            artists: Rc::new(Cell::new(false)),
            from,
        }
    }

    /// Whether the cards are artists, whose subtitles leave "Artist" out.
    pub fn set_artists(&self, artists: bool) {
        self.artists.set(artists);
    }

    /// Shows `collections`; `same` keeps the scroll, for a refresh of the same list.
    pub fn set(&self, collections: &[Collection], mixed: bool, same: bool) {
        let keep = same.then(|| self.root.vadjustment().value());
        let artists = self.artists.get();
        let items: Vec<glib::Object> = collections
            .iter()
            .map(|c| {
                glib::BoxedAnyObject::new(Entry {
                    collection: c.clone(),
                    mixed,
                    artists,
                })
                .upcast()
            })
            .collect();
        let n = self.store.n_items();
        self.store.splice(0, n, &items);
        let adj = self.root.vadjustment();
        match keep {
            Some(v) => {
                glib::idle_add_local_once(move || adj.set_value(v));
            }
            None => adj.set_value(0.0),
        }
    }

    pub fn selected(&self) -> Option<usize> {
        let s = self.selection.selected();
        (s != gtk::INVALID_LIST_POSITION).then_some(s as usize)
    }

    /// Opens the selected card's collection.
    pub fn open_selected(&self) {
        if let Some(i) = self.selected() {
            open(&self.store, i as u32, self.from);
        }
    }

    /// Gives the grid the keyboard on its selected card, else the first.
    pub fn focus(&self) {
        let i = self.selected().unwrap_or(0);
        if self.store.n_items() == 0 {
            self.grid.grab_focus();
            return;
        }
        self.selection.set_selected(i as u32);
        let _ = self
            .grid
            .activate_action("list.scroll-to-item", Some(&(i as u32).to_variant()));
    }

    pub fn focused(&self) -> bool {
        self.grid
            .root()
            .and_then(|r| r.focus())
            .is_some_and(|f| f == self.grid || f.is_ancestor(&self.grid))
    }

    /// Covers of bound cards still loading.
    pub fn covers_loading(&self) -> bool {
        let mut child = self.grid.first_child();
        while let Some(c) = child {
            if let Some(holder) = c.first_child().and_downcast::<gtk::Box>()
                && attached::<super::art::Cover>(&holder, "cover").is_some_and(|cv| cv.is_loading())
            {
                return true;
            }
            child = c.next_sibling();
        }
        false
    }
}

fn open(store: &gio::ListStore, pos: u32, from: Place) {
    let Some(obj) = store.item(pos).and_downcast::<glib::BoxedAnyObject>() else {
        return;
    };
    let collection = obj.borrow::<Entry>().collection.clone();
    super::ui::with(|ui| ui.open_collection(collection, from));
}
