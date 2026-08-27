# Журнал полного чтения Rust-кода — Codex — 2026-08-26

Автор аудита: **Codex**.

Этот журнал фиксирует последовательное чтение всех отслеживаемых Git файлов `*.rs`: файл открывается от первой строки до EOF, без структурного поиска и без выборочного пропуска тел функций или тестов. Запись `[x]` добавляется только после полного чтения соответствующего файла.

- Зафиксированный объём до начала чтения: **107 файлов, 122 884 строки**.
- Включено: production-код, модульные тесты, интеграционные/E2E-тесты и общий тестовый harness.
- Исключено: содержимое `target/` и прочие неотслеживаемые/генерируемые файлы.
- Во время полного линейного Rust-прохода остальные корневые аудиторские отчёты
  не открывались. После завершения прохода, 2026-08-27, по прямому запросу
  пользователя отдельно прочитан и перепроверен `DEEPSEEK-AUDIT-2026-08-19.md`.

## Покрытие

### `php-lsp-completion` — 5/5 файлов, 3 929/3 929 строк

- [x] `server/crates/php-lsp-completion/src/context.rs` — 582 строки — SHA-256 `21b4859f9b44d78d835a3b3ff8c42abdbc0abfdb8a019558315cf83d61e4e53b`
- [x] `server/crates/php-lsp-completion/src/context_tests.rs` — 498 строк — SHA-256 `0d51150e95d248db7658c549ae8bdf2af9a51ec7e1249742d7031317ea719f54`
- [x] `server/crates/php-lsp-completion/src/lib.rs` — 7 строк — SHA-256 `9dc6835d50c1d84b96bae80bb1328c4ea4fd963851e44d7707ada2a6c1857ffe`
- [x] `server/crates/php-lsp-completion/src/provider.rs` — 1 057 строк — SHA-256 `b4043862d20af93f8f34c33c8c7ae5e40ed713840f7ae05472597a9db80c8b2c`
- [x] `server/crates/php-lsp-completion/src/provider_tests.rs` — 1 785 строк — SHA-256 `3abeba87339e78ba38fe146be8da121d4d9eae1999f199ba8feaaba961d6b9da`

### `php-lsp-index` — 9/9 файлов, 5 518/5 518 строк

- [x] `server/crates/php-lsp-index/src/cache.rs` — 637 строк — SHA-256 `11a7a57178b1e9ab2872c7644ec77d5fbb3bcec3bf5606a42bb7bc6877aab40a`
- [x] `server/crates/php-lsp-index/src/cache_tests.rs` — 820 строк — SHA-256 `9772777716e154a3965b32aed98464f4fe3dbda59bef2d400673370b163e0e92`
- [x] `server/crates/php-lsp-index/src/composer.rs` — 174 строки — SHA-256 `99b5ac73f432917be644110a5e6ded830dd2f2229d37ac6aa4adece4962b3a76`
- [x] `server/crates/php-lsp-index/src/composer_tests.rs` — 193 строки — SHA-256 `6f0ff7232fdccd18fa96e1f69b0698b752e3ed88a43a21236d890700623362cd`
- [x] `server/crates/php-lsp-index/src/lib.rs` — 9 строк — SHA-256 `6a399f07c8459bfe4f7b5379a0a5ae8d52f08e01306b8cbe7ac0a8d1fb852263`
- [x] `server/crates/php-lsp-index/src/stubs.rs` — 333 строки — SHA-256 `fa9491ea6cd5d90944cd130cd959cff2994c05351737682dbf8d35ed815b6e0d`
- [x] `server/crates/php-lsp-index/src/stubs_tests.rs` — 395 строк — SHA-256 `05a1bbe0146989d2886988cdedcd0c3d658f81e7e9dbcb8bc121eff4f8002157`
- [x] `server/crates/php-lsp-index/src/workspace.rs` — 1 449 строк — SHA-256 `a0585fc845999f8d1463bdbb69fa66ffb9e6c5077679511b897b82c624a16591`
- [x] `server/crates/php-lsp-index/src/workspace_tests.rs` — 1 508 строк — SHA-256 `305112744e9182c837bdf1ea777118d2c74df66bbfe93965d49a1ae7e057655a`

### `php-lsp-parser` — 25/25 файлов, 22 713/22 713 строк

- [x] `server/crates/php-lsp-parser/src/cst.rs` — 162 строки — SHA-256 `479b94db8544cea37a7c42bc4f8c5cebdf6f59466ec58db41e26d265a3dcf8ea`
- [x] `server/crates/php-lsp-parser/src/cst_tests.rs` — 87 строк — SHA-256 `1adec79006d2ea6a66fd531c556b1b05bb4df31c05256845f4063c7ab19c57aa`
- [x] `server/crates/php-lsp-parser/src/diagnostics.rs` — 56 строк — SHA-256 `2b7e0603a11aaa7d8ca9620f26c94a4f75de81101a30b94477bf66576b45719f`
- [x] `server/crates/php-lsp-parser/src/diagnostics_tests.rs` — 70 строк — SHA-256 `3076cb84e94eb1da147efe35fcf1316547a45b8ea29d4d8543f8ec5579426209`
- [x] `server/crates/php-lsp-parser/src/lib.rs` — 17 строк — SHA-256 `90d8bc9a6711c77cda0c49c36e2245e12f7d4d02966c94a74dd9bd7698b429fd`
- [x] `server/crates/php-lsp-parser/src/parser.rs` — 209 строк — SHA-256 `1fd4cdc83cd74206f0714e109ea360672c65bf91bfef13314a43a0a2057c1d50`
- [x] `server/crates/php-lsp-parser/src/parser_tests.rs` — 265 строк — SHA-256 `482637e1530a03b76f6d291da60a5b1a8c929d26e265b3986b3885b8c374ec6c`
- [x] `server/crates/php-lsp-parser/src/phpdoc.rs` — 1 545 строк — SHA-256 `cb21620ded797aa53e80c56f0b5d55bfe6344942b164ac04baf688f8ace435c9`
- [x] `server/crates/php-lsp-parser/src/phpdoc_tests.rs` — 553 строки — SHA-256 `1df909a77fb302e2c250fcfe86cd598f5f30f01c5b782030bcf5d48a8b27bce8`
- [x] `server/crates/php-lsp-parser/src/references.rs` — 1 566 строк — SHA-256 `7e622d0942d19b52495c5fd497bea1bc1d573f906f360e6bea2e7a6d626fe2bb`
- [x] `server/crates/php-lsp-parser/src/references_tests.rs` — 716 строк — SHA-256 `4d7b9727f00224cbfcb3eb69d8679a6430ae02400b258495b1ca9b5542ad4099`
- [x] `server/crates/php-lsp-parser/src/resolve.rs` — 6 558 строк — SHA-256 `222e823ec01b7c22c07a001e32e69a4c6eef52a4a9c13c44ef4b03075df021e7`
- [x] `server/crates/php-lsp-parser/src/resolve_tests.rs` — 2 786 строк — SHA-256 `79a7a91009940003a491dcfb444f292a6c5cb59540e9bdc95beb447f6a7d2c3f`
- [x] `server/crates/php-lsp-parser/src/return_type.rs` — 126 строк — SHA-256 `02bb162135284f606896df71a4cf8ed1081a8d07ee93b6af3d38e056e1809284`
- [x] `server/crates/php-lsp-parser/src/return_type_tests.rs` — 49 строк — SHA-256 `3ce006a777168f57765dae39ec8015c7d7d5f3f3bd4d69d5fd38ca750336fb6b`
- [x] `server/crates/php-lsp-parser/src/semantic.rs` — 1 824 строки — SHA-256 `d46e47a48519269ce00e50fbe3b34de8189f063e02a57f71efe11b354cc26974`
- [x] `server/crates/php-lsp-parser/src/semantic_tests.rs` — 1 583 строки — SHA-256 `f1c725cbd86d7683ccd963054f6bdfa4539be966ce7cc6a543bb185269d5b921`
- [x] `server/crates/php-lsp-parser/src/semantic_tokens.rs` — 633 строки — SHA-256 `f697abfd5dfbe9ba0a260a41e0aff56b0dabab41735a4c6c0fd29b04be51d829`
- [x] `server/crates/php-lsp-parser/src/semantic_tokens_tests.rs` — 95 строк — SHA-256 `7d5279a6583f5cfbb4d62828af6c36741a14c8e0a746f5a9b16bb8445bc1e785`
- [x] `server/crates/php-lsp-parser/src/signature_help.rs` — 149 строк — SHA-256 `dc0179ef42258d9a7efe6dab480e610bce6911f10839eb4b5118deba0d210722`
- [x] `server/crates/php-lsp-parser/src/signature_help_tests.rs` — 50 строк — SHA-256 `ec01e2844864e3f37bf7d951c9c13709014dc561698c43983129b3cf29f52302`
- [x] `server/crates/php-lsp-parser/src/symbols.rs` — 2 168 строк — SHA-256 `9ae3afef08cf22fb98f199603d6273c92fa741a150e5102300a839cb70037370`
- [x] `server/crates/php-lsp-parser/src/symbols_tests.rs` — 1 117 строк — SHA-256 `8b727c52e10962f05c297f187e607fe3c8327c1bcccf35a16577f1287337f23e`
- [x] `server/crates/php-lsp-parser/src/utf16.rs` — 164 строки — SHA-256 `1a734ea30c02ac5189956987f766819500df5bca1cdd7640b1047326c47b1ca6`
- [x] `server/crates/php-lsp-parser/src/utf16_tests.rs` — 165 строк — SHA-256 `941a6c9022ea99b0d6ba5ca1f86b84590117a72d29cb16edb6e5e871fd8ec0f5`

### `php-lsp-server/src` — 49/49 файлов, 59 831/59 831 строк

- [x] `server/crates/php-lsp-server/src/analyze.rs` — 1 042 строки — SHA-256 `3cfa1ded7ae498ab876caee9770cc2ff81ffd8159bdbdd8ea4d23fcb943fc757`
- [x] `server/crates/php-lsp-server/src/analyze_tests.rs` — 462 строки — SHA-256 `e0cf6988c955ae42a030453c72e888fafeb8450600c3c89362c5493177c73fe2`
- [x] `server/crates/php-lsp-server/src/config.rs` — 362 строки — SHA-256 `cb731f96731b0e3e723dbe165cd00f990851b4f4f2a3068dfb729b144198d37f`
- [x] `server/crates/php-lsp-server/src/config_tests.rs` — 66 строк — SHA-256 `3dd1072d87b30a3238bf584b60fac03a15032b87090ac22bee0f8db1056c93df`
- [x] `server/crates/php-lsp-server/src/fix.rs` — 995 строк — SHA-256 `c8e02eb0f95a25e6990dc0e6a754b6120993311f7e10544808cfd9303b5b661f`
- [x] `server/crates/php-lsp-server/src/fix_tests.rs` — 149 строк — SHA-256 `22b602653a9e3cb1d65feab6ceb3e11df66e14b681ae401d8c77ff78f217b14e`
- [x] `server/crates/php-lsp-server/src/framework.rs` — 4 004 строки — SHA-256 `2ab57d35cc955331c4203c4adf573ae1587550221cc07b18efb08b0c6fb6aa07`
- [x] `server/crates/php-lsp-server/src/framework_tests.rs` — 1 429 строк — SHA-256 `8b90eed94cdd19cd1b9f30311bcfe51361dfb49e7a2f22572b96302663b09d4d`
- [x] `server/crates/php-lsp-server/src/indexing/cache.rs` — 217 строк — SHA-256 `e0359d16686a8abcf86c9308ffd9afaece47de23f8186eb927a1340ce6d75254`
- [x] `server/crates/php-lsp-server/src/indexing/mod.rs` — 6 строк — SHA-256 `e8a6055c5abb65b8d5f0752473b966edd77b864bcb3e5e2cdc54e489083d1755`
- [x] `server/crates/php-lsp-server/src/indexing/stubs.rs` — 272 строки — SHA-256 `85d5f9c939b40f58292fb8924c00a18160355b80c19451aaea0bd263f1d090b2`
- [x] `server/crates/php-lsp-server/src/indexing/stubs_tests.rs` — 199 строк — SHA-256 `ed0514d13b7fcbaaa62c04c68db71e49c15a1f298194f6e859308bc4807514e5`
- [x] `server/crates/php-lsp-server/src/indexing/vendor.rs` — 930 строк — SHA-256 `bad7ce51acb8323c856a44cdaa70fa39d4da147be8c25fd992b13ae3069da0eb`
- [x] `server/crates/php-lsp-server/src/indexing/workspace.rs` — 2 493 строки — SHA-256 `9d143798764075514619a9ce6c098a3503e1aa23f484f3a19894a97e1fa390c1`
- [x] `server/crates/php-lsp-server/src/indexing/workspace_tests.rs` — 415 строк — SHA-256 `3373aaaa6a5d7d914361a57cd67617c835c1d20c446e21228bf29f5d058cee08`
- [x] `server/crates/php-lsp-server/src/lib.rs` — 13 строк — SHA-256 `43729f888e8a647e5d89c261f9bcb484a1624b63a9f9084a0a1eec19c68c3d7a`
- [x] `server/crates/php-lsp-server/src/lsp/code_action.rs` — 6 701 строка — SHA-256 `0aec62d22afbc1eb03ed79360029e201fdd65cee4fce29d04e1f0704b1e5d565`
- [x] `server/crates/php-lsp-server/src/lsp/completion.rs` — 1 361 строка — SHA-256 `2ea44343d2c234a298a28ce830c1dee9034c8131dfb26a9b73f022a0795b61b8`
- [x] `server/crates/php-lsp-server/src/lsp/completion_helpers.rs` — 2 802 строки — SHA-256 `92d56d0d55b2f38f61f6d2deec784ccf8fc445c289c265a299a31af20ec23c06`
- [x] `server/crates/php-lsp-server/src/lsp/conversions.rs` — 82 строки — SHA-256 `e66633c19aaf0e46ca7e39418b1a1aee3defe6eb4cb962867c14847357c3e951`
- [x] `server/crates/php-lsp-server/src/lsp/definition.rs` — 1 207 строк — SHA-256 `61609baee6172351ce1051b232462da906016675a2c56d52eceeb1de9d8156fd`
- [x] `server/crates/php-lsp-server/src/lsp/diagnostics.rs` — 4 888 строк — SHA-256 `2cbae6c78e59adfb5f8b389164e90856fa8a60b1ae2d71f217c561eb6969483c`
- [x] `server/crates/php-lsp-server/src/lsp/document_links.rs` — 333 строки — SHA-256 `6e31dbe9cd8a308ef94be44c1a14b16a5a630f1ec2bc758c1559816ffb8000c7`
- [x] `server/crates/php-lsp-server/src/lsp/document_symbols.rs` — 676 строк — SHA-256 `7e767eaf50f748ee55d96373cbdbde5966f0fc3ef119738300232260ba1cc49b`
- [x] `server/crates/php-lsp-server/src/lsp/external_command.rs` — 67 строк — SHA-256 `91666a5c77022157ac94364e0b1e9cb6cae9f619a8b8e2582987e6353c0bf3b1`
- [x] `server/crates/php-lsp-server/src/lsp/folding.rs` — 149 строк — SHA-256 `68c2de5432940571e06ce68570d03378a51f5c0de8ee6e49df041673c308277e`
- [x] `server/crates/php-lsp-server/src/lsp/formatting.rs` — 475 строк — SHA-256 `a6e3183637cc5f7935c0bedea4d6a058a751d353e3e5f8bc889f259593798a52`
- [x] `server/crates/php-lsp-server/src/lsp/formatting_tests.rs` — 40 строк — SHA-256 `990f8904382bc2abdf7b1d77e19490a51013e328c384aef87c8014124f25ca89`
- [x] `server/crates/php-lsp-server/src/lsp/hierarchy.rs` — 1 091 строка — SHA-256 `4ef402029a9bdf4a13a116509a44e6d180388b49ea635ac6ca885a9c97ef6cf2`
- [x] `server/crates/php-lsp-server/src/lsp/hover.rs` — 1 773 строки — SHA-256 `40e575b29b7add94f12a8fadc0e9507550b19bf024d1a473403db1a5df0ec967`
- [x] `server/crates/php-lsp-server/src/lsp/inlay_hints.rs` — 4 364 строки — SHA-256 `3c269c6ee1523ae8e9cda11b64690a38df8600f95ddab0eecded078217505261`
- [x] `server/crates/php-lsp-server/src/lsp/lifecycle.rs` — 176 строк — SHA-256 `cb50b5b9ba8464ebee69df2fc3c37993159c12f8dfe5e8e120632d15075bf220`
- [x] `server/crates/php-lsp-server/src/lsp/mod.rs` — 21 строка — SHA-256 `7a1b04db0e0e5cfe47a6815cfa9aa7024af1413a2364b91919a4147f363a2284`
- [x] `server/crates/php-lsp-server/src/lsp/references.rs` — 776 строк — SHA-256 `20f1237e979a0a67e9782457cb02fe7ba1723356a547fc22ca0007c4ef66f779`
- [x] `server/crates/php-lsp-server/src/lsp/rename.rs` — 661 строка — SHA-256 `382c6cedace7cbc204b41e71fcab358820a6c252d817c4e97db7aede0b9e85e6`
- [x] `server/crates/php-lsp-server/src/lsp/rename_tests.rs` — 152 строки — SHA-256 `e0c007a1e01ded065b6a0e16c71d4ae44b74e813964b1c50bab048d6c3aa2160`
- [x] `server/crates/php-lsp-server/src/lsp/semantic_tokens.rs` — 279 строк — SHA-256 `1f7b8779652e27524bd31a77b51ef3ec7343334ee80c4229912fa4daa3479402`
- [x] `server/crates/php-lsp-server/src/lsp/templates.rs` — 4 646 строк — SHA-256 `30e6bd828ffe8d81902adf20ab04a1f9b03a1c48be874c243db5db64db3a1ee2`
- [x] `server/crates/php-lsp-server/src/lsp/templates_tests.rs` — 568 строк — SHA-256 `f5a5e7fcaa02f30be6905f92da9dd3dc36ed12ac3615f7ad4b48f8b583b5c832`
- [x] `server/crates/php-lsp-server/src/main.rs` — 148 строк — SHA-256 `cff1ff2bfa99c236ba019dd0a6997992c4edbabcfdd376f1dd14d84dd36cc5a1`
- [x] `server/crates/php-lsp-server/src/main_tests.rs` — 31 строка — SHA-256 `9703b403bcb1d287cec84d9148b00100b88e527633a0177fb9bd92441a6e88ac`
- [x] `server/crates/php-lsp-server/src/server.rs` — 3 106 строк — SHA-256 `82dd9e4bf6b3ed71ed0739a3e7050fa1f0e099a28b1571cd8758e8d58a9e45a5`
- [x] `server/crates/php-lsp-server/src/server_tests.rs` — 6 716 строк — SHA-256 `3a31ab588269aeb34ef4efb0139b2f3e3dd555d60344c67972dbba41fdd8e073`
- [x] `server/crates/php-lsp-server/src/template.rs` — 2 722 строки — SHA-256 `f56d957c6741f79fbc3074923f840a8fecb98298c9a50c363c6b64191c6dba26`
- [x] `server/crates/php-lsp-server/src/template_tests.rs` — 615 строк — SHA-256 `27b7feb197c197e8796afe2baaf6872529721584e60422bca9cdfc81bb36d015`
- [x] `server/crates/php-lsp-server/src/util/lsp_text.rs` — 67 строк — SHA-256 `94f001aee6af36e6fb2c6bc7b816e303d3bfbc5ab03b894d902fcabc8818d254`
- [x] `server/crates/php-lsp-server/src/util/lsp_text_tests.rs` — 81 строка — SHA-256 `536eebcf21032bb189a9b8782345a689f2a56a9016606b3559e3c46b69a2e754`
- [x] `server/crates/php-lsp-server/src/util/mod.rs` — 2 строки — SHA-256 `0908120578c7ce3011aaf91f0bf8b3c9a631bf9b8fb9cd25d449c1b676f73a15`
- [x] `server/crates/php-lsp-server/src/util/uri.rs` — 1 строка — SHA-256 `fc5a72776ac5dc566792d0868f000ca5d094d294bd845a35b834c873fc184116`

### `php-lsp-server/tests` — 15/15 файлов, 29 995/29 995 строк

- [x] `server/crates/php-lsp-server/tests/e2e_code_actions.rs` — 4 144 строки — SHA-256 `95e537ee9fa93240ede8bf1fcf4b5fbc987dae85b09e12f12ff40e476a958a99`
- [x] `server/crates/php-lsp-server/tests/e2e_completion.rs` — 3 755 строк — SHA-256 `eded5d746ef7dbe25110d4eeb2d3ea92a8f399918540eecc72ce7148da719af8`
- [x] `server/crates/php-lsp-server/tests/e2e_definition.rs` — 1 656 строк — SHA-256 `11a7fd2229c19447221a52bdeeb7656b1e59c55d0d67e947d71474ee2bf37cc5`
- [x] `server/crates/php-lsp-server/tests/e2e_diagnostics.rs` — 1 298 строк — SHA-256 `c8f4b0def0f5feced7fdaeed873933c79b948a4a8953600c9f759af96a202a64`
- [x] `server/crates/php-lsp-server/tests/e2e_formatting.rs` — 479 строк — SHA-256 `3c5728ca600c719ccd60b04e66568a15c629664a7fe4a660ed2b6e233bd12227`
- [x] `server/crates/php-lsp-server/tests/e2e_foundation.rs` — 466 строк — SHA-256 `368c23fb86bd64b424d518ae63f76cd70664d502852f3a0772f6277ff9aaeac6`
- [x] `server/crates/php-lsp-server/tests/e2e_hierarchy.rs` — 290 строк — SHA-256 `7bdef34ef1baa26dfcfffeaaaaf9eecfc74a2def5f84dd379ec4142b43623cf1`
- [x] `server/crates/php-lsp-server/tests/e2e_hover.rs` — 4 660 строк — SHA-256 `7a3fc8242673307922e7edc869139a7bacaf82b6ed1695440f3997f32d24711a`
- [x] `server/crates/php-lsp-server/tests/e2e_indexing.rs` — 840 строк — SHA-256 `8d8d148fc0ed47cdec59aa4aa8557105ebe262b4ef94d5be538da87574d5ad85`
- [x] `server/crates/php-lsp-server/tests/e2e_initialize.rs` — 851 строка — SHA-256 `2b6a512f551a554e8dafc34ca0eb158af89daa181d4b53d1b770c21b38169574`
- [x] `server/crates/php-lsp-server/tests/e2e_ranges.rs` — 630 строк — SHA-256 `f0f97460f3c698e72c1534e0efea85d795694842073b84d05c8f72897af2986e`
- [x] `server/crates/php-lsp-server/tests/e2e_references.rs` — 2 056 строк — SHA-256 `065d51d155cbc82f71f7b528247f97d78845009a6900013ae85e306f05e553ba`
- [x] `server/crates/php-lsp-server/tests/e2e_symbols.rs` — 891 строка — SHA-256 `f24ca48d2cee1193f9565915b9c97cd856156caa6147e05c210a3c93b72a28ff`
- [x] `server/crates/php-lsp-server/tests/e2e_templates.rs` — 7 063 строки — SHA-256 `7c1d039bdf3b7f343af7c4f3bd4088b40090ae5cdda263f08ecac909240ad7a2`
- [x] `server/crates/php-lsp-server/tests/support/mod.rs` — 916 строк — SHA-256 `d601f2e1d052b5f22216cba0615c34433497837e8d2ce4c5130e897be07c9d91`

### `php-lsp-types` — 4/4 файла, 898/898 строк

- [x] `server/crates/php-lsp-types/src/lib.rs` — 653 строки — SHA-256 `649afbea6076c4589dd09a8ef27a4943b351ad824b82f5d6a74c401701263d93`
- [x] `server/crates/php-lsp-types/src/lib_tests.rs` — 147 строк — SHA-256 `d885294a80d09b7d33fc2aa631aae78d388bd98904692ec4a447a2f228bf1ba0`
- [x] `server/crates/php-lsp-types/src/uri.rs` — 60 строк — SHA-256 `414f8c662890256940ff7c20a3d358881ff894757441476d9a46275f3bb16a42`
- [x] `server/crates/php-lsp-types/src/uri_tests.rs` — 38 строк — SHA-256 `4f0c8c402c1ac870efe86cc1c41be4f239133c15a4cfbfe4c999e2f47cc44328`

Итого нового линейного прохода: **107/107 файлов, 122 884/122 884 строки**.

Статус линейного чтения: **завершено**.

## Наблюдения прохода

Наблюдения проверены в контексте вызывающего кода и существующих тестов и
перенесены в `CODEX-AUDIT-2026-08-26.md`. После последующих проверок file
discovery и полного сопоставления DeepSeek итоговый отчёт содержит 82 группы
находок: P1 — 12, P2 — 51, P3 — 19.
