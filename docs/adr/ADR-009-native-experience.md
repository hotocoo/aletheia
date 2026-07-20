# ADR-009 — Native compositor & experience layer; hosted surface first

**Status:** Accepted
**Context:** The experience layer must be intent/entity-driven, not a windows-and-menus desktop, and must not use X11/Wayland/Linux compositors as foundation.
**Decision:** Design a native modern GPU compositor and an intent/context-driven experience layer (workspaces, dynamic interfaces, semantic navigation). Because on-GPU compositing needs hardware, M1 ships a hosted experience surface (local API + minimal semantic UI) exercising the same composition model and control/explainability surfaces.
**Consequences:** Native compositor is P5. The experience layer cannot bypass capability authorization (INV-010).
