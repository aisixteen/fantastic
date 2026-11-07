# Coverage of the kiosk's own issues

Rows moved from the base's `docs/coverage.md` with the image's checks (#475). Paths are under `kiosk/os/ci/`.

| issue | behaviour | closing PR(s) | unit tests | e2e | status |
|---|---|---|---|---|---|
| #54 | Phase 9 — fantastic OS: Debian headless kiosk image | #130 | `test_a_missing_key_is_refused` in `kiosk/os/ci/test_boot_check.py`<br>`test_a_protocol_error_is_not_yet_served` in `kiosk/os/ci/test_boot_check.py`<br>`test_an_empty_value_is_a_missing_key` in `kiosk/os/ci/test_boot_check.py`<br>`test_an_error_frame_for_the_call_fails` in `kiosk/os/ci/test_boot_check.py`<br>+45 more in the same PR(s) | deferred to the e2e phase | units only |
| #163 | os: unit tests for the boot check's socket and guest-agent helpers | #333 | `test_a_socket_closed_by_qemu_fails` in `kiosk/os/ci/test_boot_check.py`<br>`test_canvas_must_be_adopted_with_the_same_pid` in `kiosk/os/ci/test_boot_check.py`<br>`test_chan_not_connectable` in `kiosk/os/ci/test_boot_check.py`<br>`test_ga_errors_are_failures` in `kiosk/os/ci/test_boot_check.py`<br>+10 more in the same PR(s) | deferred to the e2e phase | units only |
