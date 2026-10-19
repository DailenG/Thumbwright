"""Writes a .blend whose embedded thumbnail shows a red cube with a grey cone above-left.

Run with a window (a headless `-b` session saves the startup file's stock thumbnail
instead of rendering the scene):

    blender --factory-startup --python make_blend.py -- out.blend [zstd]

The thumbnail is taken from the scene camera (file_preview_type = CAMERA) when the file is
saved. A trailing `zstd` argument saves the file compressed.
"""
import sys

import bpy

args = sys.argv[sys.argv.index("--") + 1:]
out = args[0]
compress = len(args) > 1 and args[1] == "zstd"

for obj in list(bpy.data.objects):
    bpy.data.objects.remove(obj)
scene = bpy.context.scene
scene.render.engine = "BLENDER_WORKBENCH"
scene.display.shading.color_type = "MATERIAL"

red = bpy.data.materials.new("red")
red.diffuse_color = (1.0, 0.0, 0.0, 1.0)
bpy.ops.mesh.primitive_cube_add(size=2, location=(0, 0, 0))
bpy.context.active_object.data.materials.append(red)
bpy.ops.mesh.primitive_cone_add(radius1=0.8, depth=3.0, location=(-2.0, 0, 3.5))

cam = bpy.data.objects.new("cam", bpy.data.cameras.new("cam"))
scene.collection.objects.link(cam)
cam.location = (0, -8, 0)
cam.rotation_euler = (1.5707963, 0, 0)
scene.camera = cam

bpy.context.preferences.filepaths.file_preview_type = "CAMERA"


def finish():
    bpy.ops.wm.save_as_mainfile(filepath=out, compress=compress)
    print("SAVED", out, bpy.app.version_string)
    bpy.ops.wm.quit_blender()


bpy.app.timers.register(finish, first_interval=2.0)
