//! Native xdg_popup lifecycle, placement and input management.

use smithay::backend::input::InputTime;
use smithay::desktop::{PopupKind, PopupManager, PopupKeyboardGrab, PopupPointerGrab, PopupUngrabStrategy, Window, find_popup_root_surface, get_popup_toplevel_coords};
use smithay::input::Seat;
use smithay::input::pointer::{ClickGrab, Focus, MotionEvent};
use smithay::input::touch::TouchDownGrab;
use smithay::reexports::wayland_server::{Resource, protocol::{wl_seat::WlSeat, wl_surface::WlSurface}};
use smithay::utils::{IsAlive, Serial, SERIAL_COUNTER};
use smithay::wayland::seat::WaylandFocus;
use smithay::wayland::shell::xdg::PopupSurface;

use super::state::MoonshineCompositor;
use super::popup_touch::PopupTouchGrab;

impl MoonshineCompositor {
	/// Only input actions actually delivered to this client authorize a grab.
	pub(super) fn record_input_serial(&mut self, serial: Serial, surface: Option<WlSurface>) {
		if let Some(client) = surface.and_then(|surface| surface.client()) {
			self.input_serials.record(serial.into(), client.id());
		}
	}

	pub(super) fn grab_popup(&mut self, popup: PopupSurface, seat_resource: WlSeat, serial: Serial) {
		let kind = PopupKind::Xdg(popup.clone());
		let Ok(root) = find_popup_root_surface(&kind) else { popup.send_popup_done(); return };
		let owner = popup.wl_surface().client();
		let keyboard = self.seat.get_keyboard();
		let pointer = self.seat.get_pointer();
		let touch = self.seat.get_touch();
		let valid = Seat::<Self>::from_resource(&seat_resource).as_ref() == Some(&self.seat)
			&& owner.as_ref().is_some_and(|client| self.input_serials.contains(serial.into(), &client.id()))
			&& self.focused_window.as_ref().and_then(|w| w.wl_surface()).as_deref() == Some(&root)
			&& keyboard.as_ref().is_none_or(|handle| handle.with_grab(|_, grab| grab.is::<PopupKeyboardGrab<Self>>()).unwrap_or(true))
			&& pointer.as_ref().is_none_or(|handle| handle.with_grab(|_, grab| {
				grab.is::<PopupPointerGrab<Self>>() || (grab.is::<ClickGrab<Self>>()
					&& grab.start_data().focus.as_ref().is_some_and(|(surface, _)| surface.id().same_client_as(&root.id())))
			}).unwrap_or(true))
			&& touch.as_ref().is_none_or(|handle| handle.with_grab(|_, grab| {
				grab.is::<PopupTouchGrab>() || (grab.is::<TouchDownGrab<Self>>()
					&& grab.start_data().focus.as_ref().is_some_and(|(surface, _)| surface.id().same_client_as(&root.id())))
			}).unwrap_or(true));
		if !valid {
			let _ = PopupManager::dismiss_popup(&root, &kind);
			self.screen_dirty = true;
			return;
		}
		let root_focus = self.focused_window.clone().expect("validated popup root").into();
		let seat = self.seat.clone();
		let Ok(grab) = self.popups.grab_popup(root_focus, kind, &seat, serial) else { return };
		if let Some(keyboard) = keyboard {
			keyboard.set_focus(self, grab.current_grab(), serial);
			keyboard.set_grab(self, PopupKeyboardGrab::new(&grab), serial);
		}
		if let Some(pointer) = pointer {
			pointer.set_grab(self, PopupPointerGrab::new(&grab), serial, Focus::Keep);
		}
		if let Some(touch) = touch {
			touch.set_grab(self, PopupTouchGrab::new(&grab), serial);
		}
		self.popup_grab = Some(grab);
	}

	pub(super) fn dismiss_popups_for_window(&mut self, window: Option<&Window>) {
		if let Some(grab) = &mut self.popup_grab {
			grab.ungrab(PopupUngrabStrategy::All);
		}
		if let Some(root) = window.and_then(|w| w.wl_surface()) {
			let popups: Vec<_> = PopupManager::popups_for_surface(&root).collect();
			for (popup, _) in popups.into_iter().rev() {
				let _ = PopupManager::dismiss_popup(&root, &popup);
			}
		}
		self.input_serials.clear();
		self.screen_dirty = true;
		self.reconcile_popup_grab();
	}

	/// Reconcile immediately, not on the next input event: a client may close
	/// a submenu or disappear while the streamed image and pointer are idle.
	pub(super) fn reconcile_popup_grab(&mut self) {
		self.popups.cleanup();
		let Some(grab) = self.popup_grab.as_ref() else { return };
		let ended = grab.has_ended();
		let focus = grab.current_grab().filter(IsAlive::alive);
		if ended {
			self.popup_grab = None;
			self.screen_dirty = true;
			if let Some(keyboard) = self.seat.get_keyboard()
				&& keyboard.with_grab(|_, grab| grab.is::<PopupKeyboardGrab<Self>>()).unwrap_or(false) {
				keyboard.unset_grab(self);
			}
			if let Some(pointer) = self.seat.get_pointer()
				&& pointer.with_grab(|_, grab| grab.is::<PopupPointerGrab<Self>>()).unwrap_or(false) {
				pointer.unset_grab(self, SERIAL_COUNTER.next_serial(), InputTime::from_millis(self.clock.now().as_millis()));
			}
			if let Some(touch) = self.seat.get_touch()
				&& touch.with_grab(|_, grab| grab.is::<PopupTouchGrab>()).unwrap_or(false) {
				touch.unset_grab(self);
			}
		}
		if let Some(keyboard) = self.seat.get_keyboard() {
			keyboard.set_focus(self, focus, SERIAL_COUNTER.next_serial());
		}
	}

	pub(super) fn refresh_popup_pointer(&mut self) {
		let under = super::input::find_surface_at(self, self.cursor_position);
		let Some(pointer) = self.seat.get_pointer() else { return };
		if self.popup_pointer_target != under || pointer.current_focus() != under.as_ref().map(|(s, _)| s.clone()) {
			self.popup_pointer_target = under.clone();
			pointer.motion(self, under, &MotionEvent {
				location: self.cursor_position,
				serial: SERIAL_COUNTER.next_serial(),
				time: InputTime::from_millis(self.clock.now().as_millis()),
			});
			pointer.frame(self);
		}
	}
	/// Positioner coordinates are relative to the parent's window geometry,
	/// which is distinct from its buffer origin when it has shadows/decorations.
	pub(super) fn unconstrain_popup(&self, popup: &PopupSurface) {
		let kind = PopupKind::Xdg(popup.clone());
		let Ok(root) = find_popup_root_surface(&kind) else { return };
		let Some(window) = self.space.elements().find(|w| w.wl_surface().as_deref() == Some(&root)) else {
			return;
		};
		let Some(window_geometry) = self.space.element_geometry(window) else { return };
		let Some(mut target) = self.space.output_geometry(&self.output) else { return };
		target.loc -= window_geometry.loc + get_popup_toplevel_coords(&kind);
		popup.with_pending_state(|state| {
			state.geometry = state.positioner.get_unconstrained_geometry(target);
		});
	}

	/// Run after client requests, even when no frame is rendered. This also
	/// updates output enter/leave and scale information for newly mapped menus.
	pub(super) fn refresh_popups(&mut self) {
		let roots: Vec<_> = self.space.elements().filter_map(|w| w.wl_surface().map(|s| s.into_owned())).collect();
		for root in roots {
			let popups: Vec<_> = PopupManager::popups_for_surface(&root).collect();
			for (kind, _) in popups.into_iter().rev() {
				// Smithay resets the role's parent on a null-buffer unmap, while
				// the xdg_popup resource itself remains alive until destroy.
				if matches!(&kind, PopupKind::Xdg(popup) if popup.get_parent_surface().is_none()) {
					if let Some(grab) = &mut self.popup_grab {
						grab.ungrab(PopupUngrabStrategy::All);
					}
					let _ = PopupManager::dismiss_popup(&root, &kind);
					self.screen_dirty = true;
					continue;
				}
				if let PopupKind::Xdg(popup) = kind
					&& popup.with_committed_state(|state| state.is_some_and(|s| s.positioner.reactive))
				{
					self.unconstrain_popup(&popup);
					if let Err(error) = popup.send_pending_configure() {
						tracing::debug!(?error, "Popup cannot be reactively reconfigured");
					}
				}
			}
		}
		self.popups.cleanup();
		self.space.refresh();
		self.reconcile_popup_grab();
		self.refresh_popup_pointer();
	}
}
