"""Bundle a HEIC still and a QuickTime movie into an Apple Live Photo (.livp).

A .livp is a ZIP holding the still and the movie under the same stem, as iPhones write
them (IMG_0001.HEIC + IMG_0001.MOV). Entries are stored uncompressed: both are already
compressed.

usage: make_livp.py still.heic movie.mov out.livp
"""
import sys
import zipfile

still, movie, out = sys.argv[1:4]
stem = "IMG_0001"
with zipfile.ZipFile(out, "w", zipfile.ZIP_STORED) as z:
    z.write(still, stem + ".HEIC")
    z.write(movie, stem + ".MOV")
