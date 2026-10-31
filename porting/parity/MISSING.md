# MISSING — open parity tickets (module rows + TEST-MAP ids)

Every `missing:<id>` in `MODULE-MAP.md` / `TEST-MAP.md` needs a line here.
The checkers require this file to exist even when it is empty.

## Status at the T9 closeout: no open tickets

- `TEST-MAP.md`: 2,400 rows, 0 `missing:`, 0 `missing:T-UNCLASSIFIED`
  (`test-map-check --map --forbid-unclassified` PASS).
- `MODULE-MAP.md`: 107 rows, 0 `missing:`, 0 `not ported:`
  (`module-map-check --map` PASS).
- `FEATURES.md`: 920 rows, 0 `MISSING` (the 15 PARTIAL rows each carry a
  written reason and a `[pkg:]` tag; see `STATUS.md` "TURN 9 COMPLETE").

Every former ticket was removed once its row left `missing:`: the T9A/T9B
classification sets flipped their rows to `ported:`/`covered:`/`different:`, and
the G1..G4 gap packages ported the last absent behaviours (F10, F25, F11,
T-PORT, B-113, F14, F21, F34, C-24's v2 statuses, B-74, M-131, M-130..M-133,
O-168, O-169). The only remaining FEATURES PARTIAL rows are listed in STATUS.md
with their reasons; none of them is a `missing:` row in either map.

| id | description | work package |
|---|---|---|
