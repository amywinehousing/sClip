# sClip
sClip is a Windows instant replay recorder written in Rust. It saves gameplay as ProRes 422 HQ MOV clips.

Choose a replay length from 1 to 300 seconds (default 30), start replay, and press F5 to save. Clips can be shorter while the buffer fills or when the 4 GiB memory limit is reached.

GPU acceleration uses Direct3D 11 for scaling, color conversion, and the block transform. The CPU handles quantization and entropy coding. Disable GPU acceleration in Settings or pass `--cpu-only` to compare performance.

***req:***
```
  rust
```
***todo:***
```
  linux implementation
```
