"""ssw on Modal: one L4, scaling to zero when idle.

Create a Modal secret named `ssw` with `SSW_TOKEN`. Add `ARCTOS_URL`,
`ARCTOS_CF_ACCESS_CLIENT_ID`, and `ARCTOS_CF_ACCESS_CLIENT_SECRET` to
that secret when the server should use them.
"""

import subprocess
from pathlib import Path

import modal

PORT = 7800
# A live preview is one WebSocket for its whole life, and Modal ends an
# input at this timeout. Base stations reconnect, but the restream blips.
INPUT_TIMEOUT_SECONDS = 24 * 60 * 60
SCALEDOWN_WINDOW_SECONDS = 20 * 60
MAX_INPUTS = 100
SERVER_STARTUP_TIMEOUT_SECONDS = 60
NVIDIA_VULKAN_ICD = "/usr/share/vulkan/icd.d/nvidia_icd.json"

repository_root = Path(__file__).resolve().parent.parent
vulkan_icd = (
    '{"file_format_version":"1.0.1","ICD":'
    '{"library_path":"libGLX_nvidia.so.0","api_version":"1.3.0"}}'
)
image = (
    modal.Image.from_dockerfile(
        repository_root / "ssw" / "Dockerfile",
        context_dir=repository_root,
        add_python="3.12",
    )
    .run_commands(
        "mkdir -p /usr/share/vulkan/icd.d && "
        f"printf '%s\\n' '{vulkan_icd}' > {NVIDIA_VULKAN_ICD}"
    )
    # The Dockerfile's entrypoint is ssw itself, which would replace
    # Modal's runner.
    .entrypoint([])
)

app = modal.App("ssw")


@app.function(
    image=image,
    gpu="L4",
    timeout=INPUT_TIMEOUT_SECONDS,
    scaledown_window=SCALEDOWN_WINDOW_SECONDS,
    # A recording's chunks and the job that receives them have to land on
    # the same process.
    max_containers=1,
    secrets=[modal.Secret.from_name("ssw", required_keys=["SSW_TOKEN"])],
    # Mesa's software Vulkan ICD is also installed. Pin the NVIDIA one,
    # and request the graphics and video driver libraries wgpu and NVENC need.
    env={
        "NVIDIA_DRIVER_CAPABILITIES": "all",
        "VK_ICD_FILENAMES": NVIDIA_VULKAN_ICD,
        "VK_DRIVER_FILES": NVIDIA_VULKAN_ICD,
    },
)
@modal.concurrent(max_inputs=MAX_INPUTS)
@modal.web_server(PORT, startup_timeout=SERVER_STARTUP_TIMEOUT_SECONDS)
def serve():
    subprocess.Popen(["/usr/local/bin/ssw"])
