# E54 `media` (scripted)

Inferred from gaus_arta (final cuts 2:27 / 2:28 as panels) and dawee (WebAudio). A 4 s VP9 clip made by ffmpeg, served by a read-only `files` leg allowing `media/`; an audio panel runs a silent oscillator.

**PASS**, before and after a reload: the video's `currentTime` > 0.5 with `videoWidth` > 0, and the AudioContext is `running` (Chrome runs with `--autoplay-policy=no-user-gesture-required`).
