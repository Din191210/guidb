# DESIGN.md - GuiDB Studio

## Design Read
Reading this as: Precision multi-model database studio (Operate mode) for software engineers and DBAs, in a Swiss industrial instrument style, dial ENERGY 2 / RHYTHM 2 / MOTION 1.

## Dial Rationale & Visual Decisions (R-31)
- **Layout**: Asymmetric 3-pane studio shell inspired by SQLyog (left Object Explorer tree, top-right SQL/Command Editor and Visual Query Builder, bottom-right multi-tab Data Grid / Structure / ERD / Schema Sync / Audit History) because DBAs inspect schema hierarchy, author queries, and edit tabular or document results simultaneously.
- **Color Palette (2 neutrals + 1 cobalt accent + semantic mutation states)**:
  - Light mode uses `#F8F9FA` workspace ground, `#FFFFFF` active surfaces, `#EEF1F5` grid headers, `#111827` primary text (17.74:1), `#4B5563` metadata text (7.56:1), `#64748B` control boundaries (4.76:1), and `#1D4ED8` Cobalt primary action accent (`#FFFFFF` label, 6.70:1).
  - Dark mode uses `#0B0F17` workspace ground, `#111827` active surfaces, `#1E293B` grid headers, `#F8FAFC` primary text (16.96:1), `#94A3B8` metadata text (6.92:1), `#64748B` control boundaries (3.73:1 >= 3.0:1 non-text), `#60A5FA` link/focus accent (6.98:1), and `#1D4ED8` solid action buttons.
  - Staged mutation feedback pairs high-contrast tints with explicit text badges (`+ INSERT`, `~ UPDATE`, `- DELETE`) so state never relies on color alone.
- **Typography**: System UI sans-serif stack (`"Segoe UI", system-ui, -apple-system, sans-serif`) for crisp native rendering across desktop/web without third-party render-blocking font stylesheets, paired with monospace (`ui-monospace, "Cascadia Code", "Fira Code", monospace`) strictly for SQL queries, cell identifiers, DDL, and JSON/KV values with `font-variant-numeric: tabular-nums`.
- **Spacing & Elevation**: Compact industrial spacing (`8px` base unit, `1px solid` structural borders instead of heavy drop shadows, `44px` minimum tap height on interactive controls on touch/mobile viewports).
- **Motion (MOTION 1)**: Subtle hover/focus transitions (`120ms ease`) and clean panel toggling only, disabled automatically under `prefers-reduced-motion: reduce`.
- **Icons**: Consistent 1.75px stroke geometric inline SVG icons (`aria-hidden="true"`) paired with accessible labels. Zero emojis or decorative illustrations.

## Responsive Breakpoints
- **1440px+ (Desktop)**: Persistent 280px Object Explorer sidebar beside the split Query/Builder + Multi-tab Results workspace.
- **768px (Tablet)** & **375px (Mobile)**: Sidebar collapses behind an explicit toggle button (`Objek DB`), action toolbars wrap cleanly (`flex-wrap: wrap`), and tabular data scrolls strictly inside `.table-viewport` so document width never exceeds viewport width.
