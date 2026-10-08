# Changelog

## [0.5.0](https://github.com/mattsverse/octowatch/compare/v0.4.3...v0.5.0) (2026-10-07)


### Features

* add keyboard access to the core workflow ([4348f73](https://github.com/mattsverse/octowatch/commit/4348f73f3910952d563e951a69355e64c67a331e))
* add opt-in background startup and single-instance launches ([40cb67d](https://github.com/mattsverse/octowatch/commit/40cb67da69993847ee537fd610468dc74d42da45))
* add system and light appearance themes ([920d909](https://github.com/mattsverse/octowatch/commit/920d909b00f5965b0a50ee136b7610fe44abd0dc))
* choose snooze duration for individual pull requests ([d5e6762](https://github.com/mattsverse/octowatch/commit/d5e6762aa898dd186d270195a457b4cf4d90b9d5))
* choose snooze duration for individual pull requests ([15ced40](https://github.com/mattsverse/octowatch/commit/15ced4040b6b29934d49e5bdf4c6f3278b521e7d))
* finish review notification controls and delivery ([b19f51f](https://github.com/mattsverse/octowatch/commit/b19f51f2a6e0d65261e9feec90494a866acec03e))
* monitor multiple GitHub accounts independently ([1776e10](https://github.com/mattsverse/octowatch/commit/1776e107df33c781ba0a24bda5d91d6f99839b2f))
* refresh watched repository discovery automatically ([056334d](https://github.com/mattsverse/octowatch/commit/056334df8d6f3d0d32ef201406913cf5461433e4))
* search and filter pending reviews ([fb422a1](https://github.com/mattsverse/octowatch/commit/fb422a1984d967e5a8fc419cb60b2f3bc81f26c9))
* show setup readiness and sync health ([4a5dafd](https://github.com/mattsverse/octowatch/commit/4a5dafd9d979c878d5d0639c6d20f61a0b545e8d))
* support GitHub Enterprise hosts ([8f37317](https://github.com/mattsverse/octowatch/commit/8f37317b26be89242d16fcf9424739980f4a18e5))


### Bug Fixes

* adapt Linux notification contract test to host routing ([f5fbd8f](https://github.com/mattsverse/octowatch/commit/f5fbd8f42305552d8f5737c873dcf30577e8b30f))
* address background startup review findings ([5ce865f](https://github.com/mattsverse/octowatch/commit/5ce865ff3a12b86d655f8b7716b7412af5132a7b))
* bound GitHub CLI pipe reads by request deadline ([c446520](https://github.com/mattsverse/octowatch/commit/c446520f22b30afd5d64abeac604f147d7446588))
* bound streamed Git config lines ([5386387](https://github.com/mattsverse/octowatch/commit/5386387ae418aaddb63e11eb84c1e81bfdd26976))
* confirm saved reviews before launch delivery ([ddb383d](https://github.com/mattsverse/octowatch/commit/ddb383dca1d5f2c9b376497ab997653b044bb95f))
* continue cached repository checks when discovery fails ([a0790f0](https://github.com/mattsverse/octowatch/commit/a0790f052d7477d233c321aa688d849daf2c2f53))
* defer notification window reopening until view updates finish ([d452e69](https://github.com/mattsverse/octowatch/commit/d452e69707b8feffbebc94d4f1ea664d3d9e4915))
* discover GitHub SSH-over-443 clones ([8a3e432](https://github.com/mattsverse/octowatch/commit/8a3e4329dc2e510cc320c66bbfb01735e7593681))
* exclude Git config line terminators from size limit ([19e6c62](https://github.com/mattsverse/octowatch/commit/19e6c62523575e8e507f3669ff0902c7fc42845e))
* fetch the complete watched repository review queue ([6bf6932](https://github.com/mattsverse/octowatch/commit/6bf69326dbbc368f50731fcaf9956b986bfb6851))
* keep instance ownership independent of launch environment ([2145c7e](https://github.com/mattsverse/octowatch/commit/2145c7ebf5575665dfd83a85eda824da8466a191))
* keep review controls reachable and cache filtering ([a27a199](https://github.com/mattsverse/octowatch/commit/a27a19935cfb549054a481df7368aae723d8e82e))
* preserve account preferences and unavailable repository state ([55709d8](https://github.com/mattsverse/octowatch/commit/55709d8484f03da1d9cb2835e12e19549ee3977f))
* preserve control type when recovering keyboard focus ([d65c510](https://github.com/mattsverse/octowatch/commit/d65c5103886ed386c0bdec88dae7a192731d7b95))
* preserve queued refreshes and unavailable checkout state ([c18e0ba](https://github.com/mattsverse/octowatch/commit/c18e0ba45447f11c016cf334a6c3634f83b85eec))
* preserve saved state for invalid appearance values ([89c9d84](https://github.com/mattsverse/octowatch/commit/89c9d84711b34edd9c5227003053d5f2d50efd6b))
* preserve saved state for unknown appearance names ([3881f69](https://github.com/mattsverse/octowatch/commit/3881f693c46e4812185d5d787d87ec9537f89467))
* preserve stale tray reviews and snooze reminders ([9557af9](https://github.com/mattsverse/octowatch/commit/9557af907d06974d7aa2098673ed964bbd489ecd))
* resolve omitted review request events before reconciling snoozes ([83d0ff0](https://github.com/mattsverse/octowatch/commit/83d0ff0c265f0a0368a3056b6cfd4cadae3e0be3))
* retain confirmed requests and queue overlapping refreshes ([6ea67bc](https://github.com/mattsverse/octowatch/commit/6ea67bc5f81c29d5f9f0ce5f6bafa6917b31e9b7))
* retain undelivered reviews and drain successful sends ([025b844](https://github.com/mattsverse/octowatch/commit/025b844de2a45e02542b75fe21b51546cb054237))
* update Linux notification fixture for account identity ([60ded1f](https://github.com/mattsverse/octowatch/commit/60ded1f404398c8d909cb4a61d86c91a314f2e80))

## [0.4.3](https://github.com/mattsverse/octowatch/compare/v0.4.2...v0.4.3) (2026-10-07)


### Bug Fixes

* **release:** build the DMG as APFS ([9ac985e](https://github.com/mattsverse/octowatch/commit/9ac985e4246440b7c1084f53ac710da9f8a44ebe))
* **release:** build the DMG as APFS ([f4aa115](https://github.com/mattsverse/octowatch/commit/f4aa1155125ef44a7218347f1a7384d05701c9a6))

## [0.4.2](https://github.com/mattsverse/octowatch/compare/v0.4.1...v0.4.2) (2026-10-07)


### Bug Fixes

* deliver macOS notifications through the async UserNotifications API ([ec2e5f4](https://github.com/mattsverse/octowatch/commit/ec2e5f4d61d0cc27a079bdbd44cc91d5b2b3164a))
* keep tray and save errors visible after a successful check ([38d7abd](https://github.com/mattsverse/octowatch/commit/38d7abd3a6ab69fbb747fffc51f9a5b380876a45))
* keep tray and save errors visible after a successful check ([1bdd3f3](https://github.com/mattsverse/octowatch/commit/1bdd3f346b1bf3d5a3072efa30550bf3cdb17305))

## [0.4.1](https://github.com/mattsverse/octowatch/compare/v0.4.0...v0.4.1) (2026-10-06)


### Bug Fixes

* use com.matteogassend.octowatcher as the bundle identifier ([9f618eb](https://github.com/mattsverse/octowatch/commit/9f618eb10bcf8bdc91a6a0e3549b9e99cadbb1c4))

## [0.4.0](https://github.com/mattsverse/octowatch/compare/v0.3.0...v0.4.0) (2026-10-05)


### Features

* refresh from the app and tray menus ([d6802a3](https://github.com/mattsverse/octowatch/commit/d6802a3a4afb0c42068231254bbf8f67db605223))
* refresh from the app and tray menus ([db56cb7](https://github.com/mattsverse/octowatch/commit/db56cb77aa6c7147a3d89083cbb175136e5ff1f0))


### Bug Fixes

* request macOS notification permission at startup ([6368782](https://github.com/mattsverse/octowatch/commit/63687823352420a47bfabba5730c619b900531be))

## [0.3.0](https://github.com/mattsverse/octowatch/compare/v0.2.0...v0.3.0) (2026-10-05)


### Features

* snooze review requests ([573f3c9](https://github.com/mattsverse/octowatch/commit/573f3c907165d7980f4c4e8ec15b9db3aba97b7e))
* snooze review requests ([6279ef2](https://github.com/mattsverse/octowatch/commit/6279ef21de57c75304f6b8a524bd5f177a657b6a))
* update dialogs ([363036b](https://github.com/mattsverse/octowatch/commit/363036b8ef56e9fa2fb904b7bbaedcea5d07c560))

## [0.2.0](https://github.com/mattsverse/octowatch/compare/v0.1.0...v0.2.0) (2026-10-05)


### Features

* configurable check interval ([02917ff](https://github.com/mattsverse/octowatch/commit/02917ff50b1ca43263f60a9198dad28684312e10))

## 0.1.0 (2026-10-05)


### Features

* initial ([7bf9e28](https://github.com/mattsverse/octowatch/commit/7bf9e2814bed451a80a85809f6ef956db8898ce2))
* tray ([3211ea2](https://github.com/mattsverse/octowatch/commit/3211ea20029d3962774313508301fa45f1a3f0c5))
