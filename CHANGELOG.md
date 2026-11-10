# Changelog

All notable changes to TommyBot are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/). TommyBot has no version numbers yet, so changes are grouped by pull request under Unreleased. Changes before #134 are in the merged pull requests on GitHub.

## [Unreleased]

### Added
- Staff notes keep their full edit history: the dashboard Notes page shows when a note was created and by whom, and a History list with the time, the editor and the old and new text of every edit. Discord still shows the note with its original time. (#134)
- Adding, editing, removing and sweeping notes is logged to the private mod log; an edit shows what changed. (#134)
- Dashboard Notes page has a Sweep server button behind a confirm dialog. (#134)

### Changed
- Number settings share one set of limits in the prefix panels, the dashboard and on save and import: anti-nuke Minute Cap 2–50, Hour Cap 2–500, Heat 1–100, and minimum account age 5 seconds to 100 days. The dashboard +/- buttons stop at the bounds, and a value outside a range is refused with a message instead of being changed silently. (#134)
- Saved values from before the limits keep working: a saved cap or minimum account age outside its range is read as the nearest limit, a saved heat of 0 means the card's default, and an unchanged old value never blocks a save or import. (#134)
- `jg 3c` accepts a bare number as days (`3` = 3 days) and still honours units like `12h`. (#134)
- A server-wide note sweep always asks for confirmation, even when confirmations are turned off. (#134)
- `ancaps` and the other settings panels answer an unknown card with the real range, for example "Options available are [1 to 7]", and name the letters a card has, instead of only saying the panel has no such setting. (#134)
