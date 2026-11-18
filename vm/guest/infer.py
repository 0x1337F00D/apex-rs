#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-only
"""End-to-end check: classify the Coral test parrot on every Edge TPU through
Google's unmodified libedgetpu / pycoral stack, and time it."""
import statistics
import sys
import time

from PIL import Image
from pycoral.adapters import classify, common
from pycoral.utils.dataset import read_label_file
from pycoral.utils.edgetpu import list_edge_tpus, make_interpreter

D = "/opt/coral/"
MODEL = D + "mobilenet_v2_1.0_224_inat_bird_quant_edgetpu.tflite"
EXPECT = "Ara macao (Scarlet Macaw)"
runs = int(sys.argv[1]) if len(sys.argv) > 1 else 200

tpus = list_edge_tpus()
print("edge tpus:", tpus)
assert len(tpus) >= 1, "libedgetpu found no device"
labels = read_label_file(D + "inat_bird_labels.txt")
ok = True
for i, tpu in enumerate(tpus):
    interp = make_interpreter(MODEL, device=f":{i}")
    interp.allocate_tensors()
    size = common.input_size(interp)
    common.set_input(interp, Image.open(D + "parrot.jpg").convert("RGB").resize(size, Image.LANCZOS))
    times = []
    for _ in range(runs):
        t0 = time.perf_counter()
        interp.invoke()
        times.append((time.perf_counter() - t0) * 1000)
    top = classify.get_classes(interp, top_k=1)[0]
    label = labels.get(top.id, top.id)
    good = label == EXPECT and top.score > 0.7
    ok &= good
    print(f"{'PASS' if good else 'FAIL'} :{i} {tpu['path']} -> {label} {top.score:.5f}; "
          f"first {times[0]:.2f} ms, median {statistics.median(times[1:]):.3f} ms, "
          f"p99 {sorted(times[1:])[int(0.99 * (len(times) - 2))]:.3f} ms over {runs - 1}")
    del interp
sys.exit(0 if ok else 1)
