# live — the shell over a real in-proc kernel

Seeds a fresh directory on the first key, boots the rust kernel in this process,
and shows the tree with the specified agents in it.

```script
wait 1200
shot 1_attract
key space
wait 1400
shot 2_minimized
wait 2500
shot 3_tree
key down
key down
key enter
wait 600
shot 4_selected
key tab
key tab
type ls -la
wait 300
shot 5_composer
```
