# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- Update `xtra` to [upstream](https://github.com/Restioson/xtra). This involved re-implementing some of the features
  in [comit-network's fork](https://github.com/comit-network/xtra) internally. Xtra message handler metrics were also
  removed in favour of the new `instrumentation` feature combined with
  [Grafana Tempo's span metrics](https://grafana.com/docs/tempo/latest/server_side_metrics/span_metrics/).
- Replace intermediate confirmation step in rollover protocol for the maker with a configurable flag which can be updated during runtime using the new `POST /rollover-config` endpoint.
  Rollovers are accepted by default on startup.

### Added

- Add new argument to the maker: `ignore-migration-errors`. If enabled, the maker will start if an error occurred when opening the database, if not, it will fail fast. This can come handy to prevent accidentally creating a new empty database in case database migration was unsuccessful.
- New metrics: the maker tracks how many offers have been sent (`offer_messages_sent_total`) and the taker tracks how many offers have been received (`offer_messages_received_total`).

## [0.4.21] - 2022-06-27

### Added

- Allow maker to provide extended private key as argument when starting. This key will be used to derive the internal wallet according to (Bip84)[https://github.com/bitcoin/bips/blob/master/bip-0084.mediawiki]
- Added new HTTP endpoint to manually trigger a wallet sync under `/api/sync`
- Added manual wallet sync button to the UI to allow the user to trigger a manual sync
- Added new endpoint to maker and taker to get the daemon version under `/api/version`
- Added an info box in taker-ui to show if a new version is available

### Changed

- Migrate away from JSON blobs in the DB to a more normalized database for RolloverCompleted events
- Use sled database for wallet. The wallet file is stored in your data-dir as either `maker-wallet` for the maker and `taker-wallet` for the taker respectively
- Rollover using the `libp2p` connection
- Improve the rollover protocol for resilience. Allow rollovers from a previous `commit-txid` and record a snapshot of the complete fee after each rollover
- Collaborative settlement using the `libp2p` connection
- Improve the collaborative settlement protocol for resilience. Exchange signatures to allow the taker to publish the collaborative settlement transaction at the end of the protocol.

### Fixed

- An issue where the stored revoke commit adaptor signature was not stored correctly. We now correctly pick the adaptor signature from the previous `Dlc`.

## [0.4.20] - 2022-05-26

## [0.4.19] - 2022-05-25

### Added

- A QR code for the ItchySats wallet that can be scanned to retrieve the current address.
- `ITCHYSATS_ENV` variable that defines different environment of running ItchySats (Umbrel, RaspiBlitz, Docker, Binary)

### Changed

- The API for retrieving offers now returns `leverage_details` which is a set of leverages including pre-computed values for margin, liquidation price and initial funding fee.
- Taker can choose a leverage provided the maker offers a selection of leverage.
- Rollback that a second connection of a taker would steal the old connection
- The rollover protocol is handled over the libp2p connection. The maker supports both legacy and libp2p requests for rolling over.

### Fixed

- An issue where the maker's libp2p ping actor panicked upon startup.
- An issue where fees were not handled correctly in `maia` that could result in invalid transactions.

## [0.4.17] - 2022-05-23

- Culling old DLC data from database, i.e. we remove old DLC data which is not needed anymore. This reduces the db size and is more efficient when loading

## [0.4.16] - 2022-05-13

### Changed

- Taker only re-establishes connection to maker after three minutes.
- Automatically time out `xtra` handlers after 2 minutes.

## [0.4.15] - 2022-05-12

### Changed

- Taker UI: Layout fixes and move the taker-id into a separate modal.

### Fixed

- An issue where the `payout` and `P/L` for closed positions was returned incorrectly (based on the current price).
- An issue where the position metrics recorded an error for CFDs with a weird combinations of events.
- An issue where a CET for a liquidated position could not be published because a `Txout` with a value of `0`.

## [0.4.14] - 2022-05-10

### Added

- Additional API for the taker: `api/metrics` will return prometheus-based metrics about your positions and about general behavior of the application

## [0.4.13] - 2022-05-10

### Added

- Maker only: Add new metric under `{maker_url}/api/metrics` which shows the quantity of total positions
- Maker only: Add new metric under `{maker_url}/api/metrics` which shows the number of total positions
- Taker UI: Add initial onboarding tour that steps the taker through the user interface and ends in the wallet.
- Taker UI: Add link to the FAQ (frequently asked questions) documentation.
- Taker UI: Add Feedback-fish that allows users to submit feedback through a form.
- API changes: maker and taker HTTP Api changed to let the taker chose between multiple leverages
  - maker: `/api/offer`: has an additional field `leverage_choices: Vec<Leverage>`
  - taker: `/api/cfd/order`: has an additional field `leverage: Leverage`, i.e. the leverage selected by the taker
  - both: `/api/feed/long_offer` && `/api/feed/short_ofer`: instead of a single leverage it now returns a list of leverages (`leverage_choices`)
  - network: an additional field `leverage` was added to `wire::MakerToTaker::TakeOffer`.
  - network: an additional field `leverage_choices` was added to `Order` which is sent from maker to taker when he created a new offer.
    The field `leverage` was deprecated.

### Changed

- Taker UI: Navigation improvements to more easily find the wallet.
- Taker binary: Automatically open the user interface in the default browser upon startup (includes credentials).
- Move failed CFDs (CFDs that were either rejected or the contract setup failed) into a separate table.

## [0.4.12] - 2022-04-26

### Changed

- Allow to run the `taker` binary without parameters: Default the bitcoin-network subcommand to `mainnet` and apply defaults for the `mainnet` and `testnet` maker.
- Change the finality confirmations for spend transactions to `3` confirmations instead of `1`.
- Move `Closed` CFDs to a separate database table freeing up space.
  This is a performance improvement for the startup of the application.
  Note that the database file will not change in size, but the space will be re-used internally.
- Performance improvements with adding cache and optimized loading for different actors.
- Add link to the internal wallet when there is not enough balance to open a new position.
- Improve wire log messages for better context for connection logs.

### Fixed

- An issue where CET finality was not monitored if we restarted the daemon before the CET was confirmed.
  This was fixed by adding CET monitoring to the restart behaviour of the monitor actor.
- An issue where the taker daemon lost connection to the maker because the maker did not send heartbeats due to the maker's connection actor being blocked.
  This was fixed by removing all blocking calls from the connection actors in both taker and maker which should result in a more stable connection handling overall.
- An issue where the `commit_tx` URL was not displayed for a `force` close scenario.

## [0.4.11] - 2022-04-04

### Changed

- Return a 500 Internal Server Error for `/api/cfds` whilst doing the initial load from the database.
  Previously, an empty list was returned, making clients think the database is empty.
  [PR 1717](https://github.com/itchysats/itchysats/pull/1717)

### Fixed

- Severe performance issues caused by redundant re-creation of aggregates.
  Aggregates will now be cached and only new events will be applied.
  This makes anything database related much more performant.
  [PR 1722](https://github.com/itchysats/itchysats/pull/1722)

## [0.4.10] - 2022-03-28

- Add a banner celebrating us pitching at Bitcoin 2022 conference in Miami.

## [0.4.9] - 2022-03-28

## [0.4.8] - 2022-03-24

## [0.4.7] - 2022-02-28

## [0.4.6] - 2022-02-23

## [0.4.3] - 2022-02-14

- Fix a potential deadlock in taker after losing connection to a maker by introducing timeouts for reading and writing to TCP socket.

## [0.4.2] - 2022-02-08

## [0.4.1] - 2022-02-04

## [0.4.0] - 2022-01-31

### Added

- Off-chain perpetual CFD rollover.
  Hourly, the taker sends a rollover request to the maker for each open position.
  The maker can accept or reject a rollover proposal.
  Upon acceptance the taker and maker collaboratively agree on an oracle price event further in the future and generate new payout transactions accordingly.
  The previous payout transactions are invalidated.
  The new payout transactions spend from the same lock transaction, so the rollover happens off-chain.
  In case a maker rejects a rollover request from a taker the old oracle price event and payout transactions stay in place.
- Basic authentication for the web interface of the `taker` binary.
  A password is now required to access the web interface.
  If not set via `--password`, one will be derived from the seed file and displayed in the start-up logs.

### Changed

- Username for HTTP authentication to `itchysats`.

## [0.3.6] - 2022-01-05

Backport of <https://github.com/itchysats/itchysats/pull/1016>.

## [0.3.5] - 2021-12-30

Backport of <https://github.com/itchysats/itchysats/pull/987>.

## [0.3.4] - 2021-12-30

### Added

- Proposed settlement price in case of an incoming settlement proposal.

## [0.3.3] - 2021-12-29

Backport of <https://github.com/itchysats/itchysats/pull/982>.

Backport of <https://github.com/itchysats/itchysats/pull/972>.

Backport of <https://github.com/itchysats/itchysats/pull/977>.

Backport of <https://github.com/itchysats/itchysats/pull/971>.

## [0.3.2] - 2021-12-21

Backport <https://github.com/itchysats/itchysats/pull/927> in an attempt to fix <https://github.com/itchysats/itchysats/issues/759>.

## [0.3.1] - 2021-12-20

Backport <https://github.com/itchysats/itchysats/pull/924> in an attempt to fix <https://github.com/itchysats/itchysats/issues/759>.

## [0.3.0] - 2021-12-09

Initial release for mainnet.

[Unreleased]: https://github.com/itchysats/itchysats/compare/0.4.21...HEAD
[0.4.21]: https://github.com/itchysats/itchysats/compare/0.4.20...0.4.21
[0.4.20]: https://github.com/itchysats/itchysats/compare/0.4.19...0.4.20
[0.4.19]: https://github.com/itchysats/itchysats/compare/0.4.17...0.4.19
[0.4.17]: https://github.com/itchysats/itchysats/compare/0.4.16...0.4.17
[0.4.16]: https://github.com/itchysats/itchysats/compare/0.4.15...0.4.16
[0.4.15]: https://github.com/itchysats/itchysats/compare/0.4.14...0.4.15
[0.4.14]: https://github.com/itchysats/itchysats/compare/0.4.13...0.4.14
[0.4.13]: https://github.com/itchysats/itchysats/compare/0.4.12...0.4.13
[0.4.12]: https://github.com/itchysats/itchysats/compare/0.4.11...0.4.12
[0.4.11]: https://github.com/itchysats/itchysats/compare/0.4.10...0.4.11
[0.4.10]: https://github.com/itchysats/itchysats/compare/0.4.9...0.4.10
[0.4.9]: https://github.com/itchysats/itchysats/compare/0.4.8...0.4.9
[0.4.8]: https://github.com/itchysats/itchysats/compare/0.4.7...0.4.8
[0.4.7]: https://github.com/itchysats/itchysats/compare/0.4.6...0.4.7
[0.4.6]: https://github.com/itchysats/itchysats/compare/0.4.3...0.4.6
[0.4.2]: https://github.com/itchysats/itchysats/compare/0.4.1...0.4.2
[0.4.1]: https://github.com/itchysats/itchysats/compare/0.4.0...0.4.1
[0.4.0]: https://github.com/itchysats/itchysats/compare/0.3.6...0.4.0
[0.3.6]: https://github.com/itchysats/itchysats/compare/0.3.5...0.3.6
[0.3.5]: https://github.com/itchysats/itchysats/compare/0.3.4...0.3.5
[0.3.4]: https://github.com/itchysats/itchysats/compare/0.3.3...0.3.4
[0.3.3]: https://github.com/itchysats/itchysats/compare/0.3.2...0.3.3
[0.3.2]: https://github.com/itchysats/itchysats/compare/0.3.1...0.3.2
[0.3.1]: https://github.com/itchysats/itchysats/compare/0.3.0...0.3.1
[0.3.0]: https://github.com/itchysats/itchysats/compare/d12e04d4954deb2ee9ebdc9...0.3.0
