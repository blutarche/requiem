Generated fixtures: a 6 s 440 Hz sine, stereo, ~64 kbps.

```
ffmpeg -y -f lavfi -i "sine=frequency=440:duration=6:sample_rate=48000" \
  -af "pan=stereo|c0=c0|c1=c0" -c:a libopus -b:a 64k sine440_48k.webm

ffmpeg -y -f lavfi -i "sine=frequency=440:duration=6:sample_rate=44100" \
  -af "pan=stereo|c0=c0|c1=c0" -c:a aac -b:a 64k sine440_44k.m4a

ffmpeg -y -f lavfi -i "sine=frequency=440:duration=6:sample_rate=44100" \
  -af "pan=stereo|c0=c0|c1=c0" -c:a vorbis -strict -2 -b:a 48k sine440_44k.ogg
```
