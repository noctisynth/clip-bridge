---
clip-bridge: "minor:refactor"
---

Replace the legacy X11 clipboard handler with an event-driven XFixes actor supporting TARGETS negotiation, direct and INCR transfers, bounded state, and isolated Xvfb wire tests.
