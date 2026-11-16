# Local AI model

`model/Qwen3.5-2B-CP2077-Q4_K_M.gguf` is a modified version of [Qwen3.5-2B](https://huggingface.co/Qwen/Qwen3.5-2B) by the Qwen team, licensed under the Apache License 2.0.

Changes made to the original model:
- Direct weight edits in one MLP layer (layer 15) so it explains Cyberpunk 2077 mod bugs and picks fixes more reliably.
- A short LoRA fine-tune on synthetic mod-manager bug reports, load-order questions and general answers, merged into the weights.
- Converted to GGUF and quantized to Q4_K_M with llama.cpp.

The engine in `engine/` is [llama.cpp](https://github.com/ggml-org/llama.cpp) (MIT license).
