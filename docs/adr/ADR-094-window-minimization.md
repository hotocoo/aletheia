# ADR-094 — Window minimization is presentation state

**Status:** accepted

## Context

The managed window stack already supports close, resize, maximize/restore, and keyboard focus
traversal. A desktop still needs a reversible way to remove a window from the visible composition
without destroying the application behind it.

Destroying and re-minting a window is the wrong lifecycle: close deliberately kills its surface,
input queue, and owner token. A minimized application must retain all three and its geometry.

## Decision

Add visibility as compositor presentation state. `Compositor::set_visible` is owner-token gated,
damage-aware, and does not alter the surface, token, queue, placement, or z-order.

`WindowManager::toggle_minimize` uses that primitive. Minimizing an unfocused window only hides it;
minimizing the focused window clears focus and selects the topmost visible managed survivor. Restoring
raises the window and gives it focus. Pointer hit-testing and keyboard focus traversal skip hidden
windows.

The x86-64/aarch64/RISC-V shared desktop consumes `Ctrl+Alt+M` as the minimize/restore shortcut. The
key is fed to the decoder so modifier state remains correct, but the shortcut is never delivered to
the focused application.

## Consequences

- Minimize is reversible without a new allocation or a new security authority.
- A hidden window cannot receive pointer focus.
- Closing remains destructive and retains its existing lifecycle semantics.
- Composition damage exposes the underlying stack immediately and restores the hidden surface on
  the next changed frame.
- The compositor's visibility state is deterministic and covered by owner-gated host tests.
