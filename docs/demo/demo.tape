# Records the README demo: two turns in the REPL, then roll the second one back.
#   vhs docs/demo/demo.tape      (run from the repo root, with gait on PATH)
# Makes real model calls. Waits for a turn are hidden, so they cost no GIF time;
# raise the Sleep if your model is slower than ~45s per turn.

Output docs/demo/demo.gif

Set Shell "bash"
Set Width 1200
Set Height 700
Set FontSize 18
Set TypingSpeed 40ms

Hide
Type "source docs/demo/setup.sh" Enter
Type "clear" Enter
Show

Type "gait -i" Enter
Sleep 5s   # let the banner finish before typing

Type "add a --shout flag to main.py that prints the greeting in uppercase" Enter
Sleep 5s
Hide
Sleep 45s
Show
Sleep 2s

Type "add a --times N flag that repeats the greeting N times" Enter
Sleep 5s
Hide
Sleep 45s
Show
Sleep 2s

Type "!git diff --stat" Enter
Sleep 3s

Type "/undo" Enter
Sleep 3s

Type "/undo 1" Enter
Sleep 4s
# Every undo also tells the model what changed, so it answers; wait that out.
Hide
Sleep 30s
Show
Sleep 1s

Type "!git diff --stat" Enter
Sleep 4s

Type "/exit" Enter
Sleep 1s
