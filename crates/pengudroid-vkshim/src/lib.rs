//! Android Vulkan WSI (`VK_KHR_android_surface` + DMA-BUF `VkSwapchainKHR`)
//! shim on the host GPU for PenguDroid.
//!
//! Hosted Android apps (both x86_64 and ARM64 via Berberis) create a
//! `VkSurfaceKHR` from an `ANativeWindow*` using `vkCreateAndroidSurfaceKHR`
//! and present frames via `vkCreateSwapchainKHR` / `vkQueuePresentKHR`.
//! Linux's `libvulkan.so.1` does not implement `VK_KHR_android_surface`.
//!
//! This crate exports the Vulkan loader entry points needed by
//! `libberberis_proxy_libvulkan.so` (and x86_64 apps):
//! - Intercepts `vkEnumerateInstanceExtensionProperties` /
//!   `vkEnumerateDeviceExtensionProperties` to advertise `VK_KHR_surface`,
//!   `VK_KHR_android_surface`, and `VK_KHR_swapchain`.
//! - Strips Android-only extension names in `vkCreateInstance` /
//!   `vkCreateDevice` while enabling `VK_KHR_external_memory_fd` +
//!   `VK_EXT_external_memory_dma_buf` on the host `VkDevice`.
//! - Implements `vkCreateAndroidSurfaceKHR`, `vkDestroySurfaceKHR`, and
//!   `vkGetPhysicalDeviceSurface*KHR`.
//! - Implements `vkCreateSwapchainKHR`, `vkDestroySwapchainKHR`,
//!   `vkGetSwapchainImagesKHR`, `vkAcquireNextImageKHR`,
//!   `vkAcquireNextImage2KHR`, and `vkQueuePresentKHR` backed by zero-copy
//!   DMA-BUF `VkImage` allocations sent to `pengudroid-daemon`.

#![allow(non_camel_case_types, non_snake_case, clippy::missing_safety_doc)]

use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use pengudroid_proto::{Message, PROTO_VERSION, Role, default_socket_path};
use pengudroid_socket::Connection;

// ---------------------------------------------------------------------------
// Vulkan ABI types (x86_64 64-bit)
// ---------------------------------------------------------------------------

pub type VkResult = i32;
pub type VkBool32 = u32;
pub type VkFlags = u32;
pub type VkDeviceSize = u64;
pub type VkInstance = *mut c_void;
pub type VkPhysicalDevice = *mut c_void;
pub type VkDevice = *mut c_void;
pub type VkQueue = *mut c_void;
pub type VkSurfaceKHR = u64;
pub type VkSwapchainKHR = u64;
pub type VkImage = u64;
pub type VkDeviceMemory = u64;
pub type VkSemaphore = u64;
pub type VkFence = u64;
pub type PFN_vkVoidFunction = Option<unsafe extern "C" fn()>;

const VK_SUCCESS: VkResult = 0;
const VK_NOT_READY: VkResult = 1;
const VK_SUBOPTIMAL_KHR: VkResult = 1000001003;
const VK_ERROR_OUT_OF_HOST_MEMORY: VkResult = -1;
const VK_ERROR_INITIALIZATION_FAILED: VkResult = -3;
const VK_ERROR_DEVICE_LOST: VkResult = -4;
const VK_ERROR_SURFACE_LOST_KHR: VkResult = -1000000000;

const VK_TRUE: VkBool32 = 1;
const VK_FALSE: VkBool32 = 0;

const VK_FORMAT_R8G8B8A8_UNORM: i32 = 37;
const VK_FORMAT_R8G8B8A8_SRGB: i32 = 43;
const VK_FORMAT_B8G8R8A8_UNORM: i32 = 44;
const VK_FORMAT_B8G8R8A8_SRGB: i32 = 50;
const VK_COLOR_SPACE_SRGB_NONLINEAR_KHR: i32 = 0;

const VK_PRESENT_MODE_IMMEDIATE_KHR: i32 = 0;
const VK_PRESENT_MODE_FIFO_KHR: i32 = 2;

const VK_IMAGE_TYPE_2D: i32 = 1;
const VK_IMAGE_TILING_OPTIMAL: i32 = 0;
const VK_IMAGE_TILING_LINEAR: i32 = 1;
const VK_IMAGE_LAYOUT_UNDEFINED: i32 = 0;
const VK_SHARING_MODE_EXCLUSIVE: i32 = 0;
const VK_SAMPLE_COUNT_1_BIT: u32 = 1;
const VK_IMAGE_ASPECT_COLOR_BIT: u32 = 1;
const VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT: u32 = 0x00002000;

const VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT: u32 = 0x1;
const VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT: u32 = 0x2;

const VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT: u32 = 0x00000200;

const VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO: i32 = 5;
const VK_STRUCTURE_TYPE_SUBMIT_INFO: i32 = 4;
const VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO: i32 = 14;
const VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO: i32 = 1;
const VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO: i32 = 3;
const VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO: i32 = 1000072001;
const VK_STRUCTURE_TYPE_EXPORT_MEMORY_ALLOCATE_INFO: i32 = 1000072002;
const VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO: i32 = 1000127000;
const VK_STRUCTURE_TYPE_MEMORY_GET_FD_INFO_KHR: i32 = 1000074002;

const DRM_FORMAT_ARGB8888: u32 = 0x34325241;
const DRM_FORMAT_ABGR8888: u32 = 0x34324241;
const DRM_FORMAT_MOD_LINEAR: u64 = 0;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkExtent2D {
    pub width: u32,
    pub height: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkExtent3D {
    pub width: u32,
    pub height: u32,
    pub depth: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkExtensionProperties {
    pub extensionName: [c_char; 256],
    pub specVersion: u32,
}

#[repr(C)]
pub struct VkInstanceCreateInfo {
    pub sType: i32,
    pub pNext: *const c_void,
    pub flags: VkFlags,
    pub pApplicationInfo: *const c_void,
    pub enabledLayerCount: u32,
    pub ppEnabledLayerNames: *const *const c_char,
    pub enabledExtensionCount: u32,
    pub ppEnabledExtensionNames: *const *const c_char,
}

#[repr(C)]
pub struct VkDeviceQueueCreateInfo {
    pub sType: i32,
    pub pNext: *const c_void,
    pub flags: VkFlags,
    pub queueFamilyIndex: u32,
    pub queueCount: u32,
    pub pQueuePriorities: *const f32,
}

#[repr(C)]
pub struct VkDeviceCreateInfo {
    pub sType: i32,
    pub pNext: *const c_void,
    pub flags: VkFlags,
    pub queueCreateInfoCount: u32,
    pub pQueueCreateInfos: *const VkDeviceQueueCreateInfo,
    pub enabledLayerCount: u32,
    pub ppEnabledLayerNames: *const *const c_char,
    pub enabledExtensionCount: u32,
    pub ppEnabledExtensionNames: *const *const c_char,
    pub pEnabledFeatures: *const c_void,
}

#[repr(C)]
pub struct VkAndroidSurfaceCreateInfoKHR {
    pub sType: i32,
    pub pNext: *const c_void,
    pub flags: VkFlags,
    pub window: *mut c_void,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkSurfaceCapabilitiesKHR {
    pub minImageCount: u32,
    pub maxImageCount: u32,
    pub currentExtent: VkExtent2D,
    pub minImageExtent: VkExtent2D,
    pub maxImageExtent: VkExtent2D,
    pub maxImageArrayLayers: u32,
    pub supportedTransforms: VkFlags,
    pub currentTransform: VkFlags,
    pub supportedCompositeAlpha: VkFlags,
    pub supportedUsageFlags: VkFlags,
}

#[repr(C)]
pub struct VkSurfaceCapabilities2KHR {
    pub sType: i32,
    pub pNext: *mut c_void,
    pub surfaceCapabilities: VkSurfaceCapabilitiesKHR,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VkSurfaceFormatKHR {
    pub format: i32,
    pub colorSpace: i32,
}

#[repr(C)]
pub struct VkSurfaceFormat2KHR {
    pub sType: i32,
    pub pNext: *mut c_void,
    pub surfaceFormat: VkSurfaceFormatKHR,
}

#[repr(C)]
pub struct VkSwapchainCreateInfoKHR {
    pub sType: i32,
    pub pNext: *const c_void,
    pub flags: VkFlags,
    pub surface: VkSurfaceKHR,
    pub minImageCount: u32,
    pub imageFormat: i32,
    pub imageColorSpace: i32,
    pub imageExtent: VkExtent2D,
    pub imageArrayLayers: u32,
    pub imageUsage: VkFlags,
    pub imageSharingMode: i32,
    pub queueFamilyIndexCount: u32,
    pub pQueueFamilyIndices: *const u32,
    pub preTransform: VkFlags,
    pub compositeAlpha: VkFlags,
    pub presentMode: i32,
    pub clipped: VkBool32,
    pub oldSwapchain: VkSwapchainKHR,
}

#[repr(C)]
pub struct VkPresentInfoKHR {
    pub sType: i32,
    pub pNext: *const c_void,
    pub waitSemaphoreCount: u32,
    pub pWaitSemaphores: *const VkSemaphore,
    pub swapchainCount: u32,
    pub pSwapchains: *const VkSwapchainKHR,
    pub pImageIndices: *const u32,
    pub pResults: *mut VkResult,
}

#[repr(C)]
pub struct VkAcquireNextImageInfoKHR {
    pub sType: i32,
    pub pNext: *const c_void,
    pub swapchain: VkSwapchainKHR,
    pub timeout: u64,
    pub semaphore: VkSemaphore,
    pub fence: VkFence,
    pub deviceMask: u32,
}

#[repr(C)]
struct VkImageCreateInfo {
    sType: i32,
    pNext: *const c_void,
    flags: VkFlags,
    imageType: i32,
    format: i32,
    extent: VkExtent3D,
    mipLevels: u32,
    arrayLayers: u32,
    samples: u32,
    tiling: i32,
    usage: VkFlags,
    sharingMode: i32,
    queueFamilyIndexCount: u32,
    pQueueFamilyIndices: *const u32,
    initialLayout: i32,
}

#[repr(C)]
struct VkExternalMemoryImageCreateInfo {
    sType: i32,
    pNext: *const c_void,
    handleTypes: u32,
}

#[repr(C)]
#[derive(Default)]
struct VkMemoryRequirements {
    size: VkDeviceSize,
    alignment: VkDeviceSize,
    memoryTypeBits: u32,
}

#[repr(C)]
struct VkMemoryAllocateInfo {
    sType: i32,
    pNext: *const c_void,
    allocationSize: VkDeviceSize,
    memoryTypeIndex: u32,
}

#[repr(C)]
struct VkExportMemoryAllocateInfo {
    sType: i32,
    pNext: *const c_void,
    handleTypes: u32,
}

#[repr(C)]
struct VkMemoryDedicatedAllocateInfo {
    sType: i32,
    pNext: *const c_void,
    image: VkImage,
    buffer: u64,
}

#[repr(C)]
struct VkMemoryGetFdInfoKHR {
    sType: i32,
    pNext: *const c_void,
    memory: VkDeviceMemory,
    handleType: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct VkMemoryType {
    propertyFlags: u32,
    heapIndex: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct VkMemoryHeap {
    size: VkDeviceSize,
    flags: u32,
}

#[repr(C)]
struct VkPhysicalDeviceMemoryProperties {
    memoryTypeCount: u32,
    memoryTypes: [VkMemoryType; 32],
    memoryHeapCount: u32,
    memoryHeaps: [VkMemoryHeap; 16],
}

#[repr(C)]
struct VkImageSubresource {
    aspectMask: u32,
    mipLevel: u32,
    arrayLayer: u32,
}

#[repr(C)]
#[derive(Default)]
struct VkSubresourceLayout {
    offset: VkDeviceSize,
    size: VkDeviceSize,
    rowPitch: VkDeviceSize,
    arrayPitch: VkDeviceSize,
    depthPitch: VkDeviceSize,
}

#[repr(C)]
struct VkSubmitInfo {
    sType: i32,
    pNext: *const c_void,
    waitSemaphoreCount: u32,
    pWaitSemaphores: *const VkSemaphore,
    pWaitDstStageMask: *const u32,
    commandBufferCount: u32,
    pCommandBuffers: *const *mut c_void,
    signalSemaphoreCount: u32,
    pSignalSemaphores: *const VkSemaphore,
}

// ---------------------------------------------------------------------------
// Host Vulkan Loader FFI
// ---------------------------------------------------------------------------

type PfnGetInstanceProcAddr = unsafe extern "C" fn(VkInstance, *const c_char) -> PFN_vkVoidFunction;
type PfnGetDeviceProcAddr = unsafe extern "C" fn(VkDevice, *const c_char) -> PFN_vkVoidFunction;
type PfnEnumerateInstanceExtensionProperties =
    unsafe extern "C" fn(*const c_char, *mut u32, *mut VkExtensionProperties) -> VkResult;
type PfnEnumerateDeviceExtensionProperties = unsafe extern "C" fn(
    VkPhysicalDevice,
    *const c_char,
    *mut u32,
    *mut VkExtensionProperties,
) -> VkResult;
type PfnCreateInstance =
    unsafe extern "C" fn(*const VkInstanceCreateInfo, *const c_void, *mut VkInstance) -> VkResult;
type PfnDestroyInstance = unsafe extern "C" fn(VkInstance, *const c_void);
type PfnCreateDevice = unsafe extern "C" fn(
    VkPhysicalDevice,
    *const VkDeviceCreateInfo,
    *const c_void,
    *mut VkDevice,
) -> VkResult;
type PfnDestroyDevice = unsafe extern "C" fn(VkDevice, *const c_void);
type PfnGetDeviceQueue = unsafe extern "C" fn(VkDevice, u32, u32, *mut VkQueue);
type PfnQueueSubmit = unsafe extern "C" fn(VkQueue, u32, *const VkSubmitInfo, VkFence) -> VkResult;
type PfnQueueWaitIdle = unsafe extern "C" fn(VkQueue) -> VkResult;
type PfnDeviceWaitIdle = unsafe extern "C" fn(VkDevice) -> VkResult;
type PfnGetPhysicalDeviceMemoryProperties =
    unsafe extern "C" fn(VkPhysicalDevice, *mut VkPhysicalDeviceMemoryProperties);
type PfnCreateImage = unsafe extern "C" fn(
    VkDevice,
    *const VkImageCreateInfo,
    *const c_void,
    *mut VkImage,
) -> VkResult;
type PfnDestroyImage = unsafe extern "C" fn(VkDevice, VkImage, *const c_void);
type PfnGetImageMemoryRequirements =
    unsafe extern "C" fn(VkDevice, VkImage, *mut VkMemoryRequirements);
type PfnAllocateMemory = unsafe extern "C" fn(
    VkDevice,
    *const VkMemoryAllocateInfo,
    *const c_void,
    *mut VkDeviceMemory,
) -> VkResult;
type PfnFreeMemory = unsafe extern "C" fn(VkDevice, VkDeviceMemory, *const c_void);
type PfnBindImageMemory =
    unsafe extern "C" fn(VkDevice, VkImage, VkDeviceMemory, VkDeviceSize) -> VkResult;
type PfnGetImageSubresourceLayout =
    unsafe extern "C" fn(VkDevice, VkImage, *const VkImageSubresource, *mut VkSubresourceLayout);
type PfnGetMemoryFdKHR =
    unsafe extern "C" fn(VkDevice, *const VkMemoryGetFdInfoKHR, *mut c_int) -> VkResult;

// DRM modifier images preserve the driver's native tiling. In particular,
// NVIDIA does not support linear images as color attachments.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct DrmModifierProperties {
    modifier: u64,
    planes: u32,
    features: u32,
}
#[repr(C)]
struct DrmModifierList {
    s_type: i32,
    next: *mut c_void,
    count: u32,
    properties: *mut DrmModifierProperties,
}
#[repr(C)]
struct FormatProperties2 {
    s_type: i32,
    next: *mut c_void,
    properties: [u32; 3],
}
#[repr(C)]
struct DrmImageInfo {
    s_type: i32,
    next: *const c_void,
    modifier: u64,
    sharing: i32,
    family_count: u32,
    families: *const u32,
}
#[repr(C)]
struct ExternalImageInfo {
    s_type: i32,
    next: *const c_void,
    handle_type: u32,
}
#[repr(C)]
struct ImageFormatInfo2 {
    s_type: i32,
    next: *const c_void,
    format: i32,
    image_type: i32,
    tiling: i32,
    usage: u32,
    flags: u32,
}
#[repr(C)]
struct ExternalImageProperties {
    s_type: i32,
    next: *mut c_void,
    features: u32,
    export_types: u32,
    compatible_types: u32,
}
#[repr(C)]
struct ImageFormatProperties {
    extent: VkExtent3D,
    mip_levels: u32,
    layers: u32,
    samples: u32,
    max_resource_size: u64,
}
#[repr(C)]
struct ImageFormatProperties2 {
    s_type: i32,
    next: *mut c_void,
    properties: ImageFormatProperties,
}
#[repr(C)]
struct DrmImageList {
    s_type: i32,
    next: *const c_void,
    count: u32,
    modifiers: *const u64,
}

unsafe fn exportable_drm_modifier(
    instance: VkInstance,
    physical: VkPhysicalDevice,
    format: i32,
    usage: u32,
) -> Option<u64> {
    type GetFormats = unsafe extern "C" fn(VkPhysicalDevice, i32, *mut FormatProperties2);
    type GetImages = unsafe extern "C" fn(
        VkPhysicalDevice,
        *const ImageFormatInfo2,
        *mut ImageFormatProperties2,
    ) -> VkResult;
    let formats: GetFormats = std::mem::transmute(
        host_instance_fn(instance, b"vkGetPhysicalDeviceFormatProperties2\0")
            .or_else(|| host_instance_fn(instance, b"vkGetPhysicalDeviceFormatProperties2KHR\0"))?,
    );
    let images: GetImages = std::mem::transmute(
        host_instance_fn(instance, b"vkGetPhysicalDeviceImageFormatProperties2\0").or_else(
            || host_instance_fn(instance, b"vkGetPhysicalDeviceImageFormatProperties2KHR\0"),
        )?,
    );
    let mut list = DrmModifierList {
        s_type: 1000158000,
        next: std::ptr::null_mut(),
        count: 0,
        properties: std::ptr::null_mut(),
    };
    let mut properties = FormatProperties2 {
        s_type: 1000059002,
        next: (&mut list as *mut DrmModifierList).cast(),
        properties: [0; 3],
    };
    formats(physical, format, &mut properties);
    if list.count == 0 || list.count > 4096 {
        return None;
    }
    let mut candidates = vec![DrmModifierProperties::default(); list.count as usize];
    list.properties = candidates.as_mut_ptr();
    formats(physical, format, &mut properties);
    for candidate in candidates.iter().take(list.count as usize) {
        // The daemon currently imports one DMA-BUF plane for RGBA images.
        if candidate.planes != 1 || candidate.features & 0x80 == 0 {
            continue;
        }
        let drm = DrmImageInfo {
            s_type: 1000158002,
            next: std::ptr::null(),
            modifier: candidate.modifier,
            sharing: 0,
            family_count: 0,
            families: std::ptr::null(),
        };
        let external = ExternalImageInfo {
            s_type: 1000071000,
            next: (&drm as *const DrmImageInfo).cast(),
            handle_type: VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
        };
        let info = ImageFormatInfo2 {
            s_type: 1000059004,
            next: (&external as *const ExternalImageInfo).cast(),
            format,
            image_type: VK_IMAGE_TYPE_2D,
            tiling: 1000158000,
            usage,
            flags: 0,
        };
        let mut exported = ExternalImageProperties {
            s_type: 1000071001,
            next: std::ptr::null_mut(),
            features: 0,
            export_types: 0,
            compatible_types: 0,
        };
        let mut out: ImageFormatProperties2 = std::mem::zeroed();
        out.s_type = 1000059003;
        out.next = (&mut exported as *mut ExternalImageProperties).cast();
        if images(physical, &info, &mut out) == VK_SUCCESS
            && exported.features & 2 != 0
            && exported.compatible_types & VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT != 0
        {
            return Some(candidate.modifier);
        }
    }
    None
}

struct HostLoader {
    get_instance_proc_addr: PfnGetInstanceProcAddr,
    get_device_proc_addr: PfnGetDeviceProcAddr,
    enum_instance_exts: PfnEnumerateInstanceExtensionProperties,
    create_instance: PfnCreateInstance,
    destroy_instance: PfnDestroyInstance,
    enum_device_exts: PfnEnumerateDeviceExtensionProperties,
    create_device: PfnCreateDevice,
    destroy_device: PfnDestroyDevice,
}

static HOST: OnceLock<Result<HostLoader, String>> = OnceLock::new();

fn trace_enabled() -> bool {
    std::env::var_os("PENGUDROID_TRACE_VKSHIM").is_some()
}

fn host() -> Result<&'static HostLoader, String> {
    HOST.get_or_init(|| unsafe {
        let name = b"libvulkan.so.1\0";
        let handle = libc::dlopen(
            name.as_ptr() as *const c_char,
            libc::RTLD_NOW | libc::RTLD_LOCAL,
        );
        if handle.is_null() {
            return Err("dlopen(libvulkan.so.1) failed".into());
        }
        macro_rules! load {
            ($sym:literal, $ty:ty) => {{
                let p = libc::dlsym(handle, concat!($sym, "\0").as_ptr() as *const c_char);
                if p.is_null() {
                    return Err(format!("missing symbol {} in libvulkan.so.1", $sym));
                }
                std::mem::transmute::<*mut c_void, $ty>(p)
            }};
        }
        Ok(HostLoader {
            get_instance_proc_addr: load!("vkGetInstanceProcAddr", PfnGetInstanceProcAddr),
            get_device_proc_addr: load!("vkGetDeviceProcAddr", PfnGetDeviceProcAddr),
            enum_instance_exts: load!(
                "vkEnumerateInstanceExtensionProperties",
                PfnEnumerateInstanceExtensionProperties
            ),
            create_instance: load!("vkCreateInstance", PfnCreateInstance),
            destroy_instance: load!("vkDestroyInstance", PfnDestroyInstance),
            enum_device_exts: load!(
                "vkEnumerateDeviceExtensionProperties",
                PfnEnumerateDeviceExtensionProperties
            ),
            create_device: load!("vkCreateDevice", PfnCreateDevice),
            destroy_device: load!("vkDestroyDevice", PfnDestroyDevice),
        })
    })
    .as_ref()
    .map_err(Clone::clone)
}

unsafe fn host_instance_fn(instance: VkInstance, name: &[u8]) -> Option<*mut c_void> {
    let h = host().ok()?;
    let f = (h.get_instance_proc_addr)(instance, name.as_ptr() as *const c_char)?;
    Some(f as *mut c_void)
}

unsafe fn host_device_fn(device: VkDevice, name: &[u8]) -> Option<*mut c_void> {
    let h = host().ok()?;
    let f = (h.get_device_proc_addr)(device, name.as_ptr() as *const c_char)?;
    Some(f as *mut c_void)
}

// ---------------------------------------------------------------------------
// Global State (Instances, Devices, Surfaces, Swapchains, Daemon socket)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct SurfaceRec {
    window: *mut c_void,
    width: u32,
    height: u32,
    surface_id: u32,
}
unsafe impl Send for SurfaceRec {}

struct DeviceRec {
    instance: VkInstance,
    physical_device: VkPhysicalDevice,
    drm_modifiers: bool,
    queue: VkQueue,
}
unsafe impl Send for DeviceRec {}

struct SwapchainImage {
    image: VkImage,
    memory: VkDeviceMemory,
    dmabuf_fd: OwnedFd,
    stride: u32,
    offset: u32,
    fourcc: u32,
    modifier: u64,
}

struct SwapchainRec {
    device: VkDevice,
    surface: VkSurfaceKHR,
    width: u32,
    height: u32,
    images: Vec<SwapchainImage>,
    next_index: usize,
}
unsafe impl Send for SwapchainRec {}

#[derive(Default)]
struct VkState {
    instances: HashSet<usize>,
    devices: HashMap<usize, DeviceRec>,
    surfaces: HashMap<u64, SurfaceRec>,
    swapchains: HashMap<u64, SwapchainRec>,
    next_handle: u64,
    conn: Option<Connection>,
    connect_attempted: bool,
    cached_surface_id: u32,
}

static STATE: OnceLock<Mutex<VkState>> = OnceLock::new();

fn lock_state() -> std::sync::MutexGuard<'static, VkState> {
    STATE
        .get_or_init(|| Mutex::new(VkState::default()))
        .lock()
        .unwrap()
}

unsafe fn query_native_window_size(window: *mut c_void) -> (u32, u32) {
    if !window.is_null() {
        type GetDim = unsafe extern "C" fn(*mut c_void) -> i32;
        let resolve = |name: &[u8]| -> Option<GetDim> {
            let sym = libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr() as *const c_char);
            if sym.is_null() {
                None
            } else {
                Some(std::mem::transmute::<*mut c_void, GetDim>(sym))
            }
        };
        let w = resolve(b"ANativeWindow_getWidth\0")
            .map(|f| f(window))
            .unwrap_or(0);
        let h = resolve(b"ANativeWindow_getHeight\0")
            .map(|f| f(window))
            .unwrap_or(0);
        if w > 0 && h > 0 {
            return (w as u32, h as u32);
        }
    }
    let env_u32 = |key: &str, default: u32| -> u32 {
        std::env::var(key)
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .filter(|&v| v > 0)
            .unwrap_or(default)
    };
    (
        env_u32("PENGUDROID_SURFACE_WIDTH", 1280),
        env_u32("PENGUDROID_SURFACE_HEIGHT", 720),
    )
}

fn subscribe_android_input(surface_id: u32) {
    std::env::set_var("PENGUDROID_LAST_SURFACE_ID", surface_id.to_string());
    std::env::set_var("PENGUDROID_VK_SURFACE_ID", surface_id.to_string());
    unsafe {
        let hook = libc::dlsym(libc::RTLD_DEFAULT, c"pengudroid_subscribe_input".as_ptr());
        if !hook.is_null() {
            let subscribe: unsafe extern "C" fn(u32) = std::mem::transmute(hook);
            subscribe(surface_id);
        } else {
            eprintln!("[vkshim] Android input hook unavailable for surface {surface_id}");
        }
    }
}

fn ensure_daemon_surface_id(st: &mut VkState, width: u32, height: u32) -> Result<u32, String> {
    if st.conn.is_none() {
        if st.connect_attempted {
            return Err("daemon is unavailable".into());
        }
        st.connect_attempted = true;
        let conn = Connection::connect_endpoint(&default_socket_path())
            .map_err(|e| format!("cannot connect to daemon: {e}"))?;
        let _ = conn.set_read_timeout(Some(Duration::from_millis(5000)));
        let hello = Message::Hello {
            version: PROTO_VERSION,
            role: Role::Guest,
            pid: std::process::id() as i32,
        };
        conn.send_message(&hello, &[]).map_err(|e| {
            st.connect_attempted = false;
            format!("Hello failed: {e}")
        })?;
        let (reply, fds) = conn.recv_message().map_err(|e| {
            st.connect_attempted = false;
            format!("Welcome failed: {e}")
        })?;
        drop(fds);
        match reply {
            Message::Welcome { .. } => {}
            other => {
                st.connect_attempted = false;
                return Err(format!("unexpected Welcome: {other:?}"));
            }
        }
        st.conn = Some(conn);
    }

    if st.cached_surface_id != 0 {
        return Ok(st.cached_surface_id);
    }
    if let Ok(val) = std::env::var("PENGUDROID_SURFACE_ID") {
        if let Ok(id) = val.trim().parse::<u32>() {
            if id != 0 {
                st.cached_surface_id = id;
                subscribe_android_input(id);
                return Ok(id);
            }
        }
    }

    let req = Message::CreateSurface {
        width: width.max(1),
        height: height.max(1),
        title: std::env::var("PENGUDROID_APP_LABEL").unwrap_or_else(|_| "PenguDroid".into()),
    };
    let conn = st.conn.as_ref().ok_or("missing daemon conn")?;
    conn.send_message(&req, &[])
        .map_err(|e| format!("CreateSurface send failed: {e}"))?;
    let (reply, _fds) = conn
        .recv_message()
        .map_err(|e| format!("CreateSurface recv failed: {e}"))?;
    match reply {
        Message::SurfaceCreated { surface_id, .. } => {
            st.cached_surface_id = surface_id;
            subscribe_android_input(surface_id);
            Ok(surface_id)
        }
        Message::Error { code, message } => {
            Err(format!("CreateSurface rejected ({code}): {message}"))
        }
        other => Err(format!("unexpected CreateSurface reply: {other:?}")),
    }
}

fn present_dmabuf_to_daemon(
    surface_id: u32,
    width: u32,
    height: u32,
    stride: u32,
    fourcc: u32,
    modifier: u64,
    offset: u32,
    fd: RawFd,
) -> Result<(), String> {
    let mut st = lock_state();
    let sid = if surface_id != 0 {
        surface_id
    } else {
        ensure_daemon_surface_id(&mut st, width, height)?
    };
    let conn = st.conn.take().ok_or("daemon is unavailable")?;
    let mut io_fail = false;
    let res = (|| -> Result<(), String> {
        let msg = Message::PresentDmabuf {
            surface_id: sid,
            width,
            height,
            stride,
            fourcc,
            modifier,
            offset,
            top_down: true,
        };
        conn.send_message(&msg, &[fd]).map_err(|e| {
            io_fail = true;
            format!("PresentDmabuf send failed: {e}")
        })?;
        let (reply, _fds) = conn.recv_message().map_err(|e| {
            io_fail = true;
            format!("PresentDmabuf recv failed: {e}")
        })?;
        match reply {
            Message::Ack => Ok(()),
            Message::Error { code, message } => {
                Err(format!("PresentDmabuf rejected ({code}): {message}"))
            }
            other => Err(format!("unexpected PresentDmabuf reply: {other:?}")),
        }
    })();
    if io_fail {
        st.conn = None;
        st.connect_attempted = false;
    } else {
        st.conn = Some(conn);
    }
    res
}

fn write_ext_prop(dst: &mut VkExtensionProperties, name: &str, spec: u32) {
    dst.extensionName = [0; 256];
    for (i, b) in name.bytes().take(255).enumerate() {
        dst.extensionName[i] = b as c_char;
    }
    dst.specVersion = spec;
}

fn ext_name(prop: &VkExtensionProperties) -> &str {
    let cstr = unsafe { CStr::from_ptr(prop.extensionName.as_ptr()) };
    cstr.to_str().unwrap_or("")
}

fn format_to_fourcc(format: i32) -> u32 {
    match format {
        VK_FORMAT_R8G8B8A8_UNORM | VK_FORMAT_R8G8B8A8_SRGB => DRM_FORMAT_ABGR8888,
        VK_FORMAT_B8G8R8A8_UNORM | VK_FORMAT_B8G8R8A8_SRGB | _ => DRM_FORMAT_ARGB8888,
    }
}

// ---------------------------------------------------------------------------
// Exported Vulkan Entry Points
// ---------------------------------------------------------------------------

#[no_mangle]
pub unsafe extern "C" fn vkEnumerateInstanceExtensionProperties(
    pLayerName: *const c_char,
    pPropertyCount: *mut u32,
    pProperties: *mut VkExtensionProperties,
) -> VkResult {
    if trace_enabled() {
        eprintln!("[vkshim] vkEnumerateInstanceExtensionProperties(pProperties={pProperties:p})");
    }
    let Ok(h) = host() else {
        return VK_ERROR_INITIALIZATION_FAILED;
    };
    if pPropertyCount.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let mut host_count = 0u32;
    let res = (h.enum_instance_exts)(pLayerName, &mut host_count, std::ptr::null_mut());
    if res != VK_SUCCESS {
        return res;
    }
    let mut list = vec![
        VkExtensionProperties {
            extensionName: [0; 256],
            specVersion: 0,
        };
        host_count as usize
    ];
    if host_count > 0 {
        let res = (h.enum_instance_exts)(pLayerName, &mut host_count, list.as_mut_ptr());
        if res < 0 {
            return res;
        }
        list.truncate(host_count as usize);
    }
    if pLayerName.is_null() {
        for (name, ver) in [("VK_KHR_surface", 25u32), ("VK_KHR_android_surface", 6u32)] {
            if !list.iter().any(|p| ext_name(p) == name) {
                let mut prop = VkExtensionProperties {
                    extensionName: [0; 256],
                    specVersion: 0,
                };
                write_ext_prop(&mut prop, name, ver);
                list.push(prop);
            }
        }
    }
    if pProperties.is_null() {
        *pPropertyCount = list.len() as u32;
        return VK_SUCCESS;
    }
    let cap = *pPropertyCount as usize;
    let copy_n = cap.min(list.len());
    std::ptr::copy_nonoverlapping(list.as_ptr(), pProperties, copy_n);
    *pPropertyCount = copy_n as u32;
    if copy_n < list.len() {
        5 // VK_INCOMPLETE
    } else {
        VK_SUCCESS
    }
}

#[no_mangle]
pub unsafe extern "C" fn vkCreateInstance(
    pCreateInfo: *const VkInstanceCreateInfo,
    pAllocator: *const c_void,
    pInstance: *mut VkInstance,
) -> VkResult {
    let Ok(h) = host() else {
        if trace_enabled() {
            eprintln!("[vkshim] vkCreateInstance: host() failed");
        }
        return VK_ERROR_INITIALIZATION_FAILED;
    };
    if pCreateInfo.is_null() || pInstance.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let info = &*pCreateInfo;
    let mut filtered_exts: Vec<CString> = Vec::new();
    let mut need_surface = false;
    if info.enabledExtensionCount > 0 && !info.ppEnabledExtensionNames.is_null() {
        let slice = std::slice::from_raw_parts(
            info.ppEnabledExtensionNames,
            info.enabledExtensionCount as usize,
        );
        for &ptr in slice {
            if ptr.is_null() {
                continue;
            }
            let name = CStr::from_ptr(ptr).to_string_lossy();
            if name == "VK_KHR_android_surface" {
                need_surface = true;
                continue;
            }
            if let Ok(cs) = CString::new(name.as_ref()) {
                filtered_exts.push(cs);
            }
        }
    }
    if need_surface
        && !filtered_exts
            .iter()
            .any(|s| s.as_bytes() == b"VK_KHR_surface")
    {
        filtered_exts.push(CString::new("VK_KHR_surface").unwrap());
    }
    let ext_ptrs: Vec<*const c_char> = filtered_exts.iter().map(|s| s.as_ptr()).collect();
    let patched = VkInstanceCreateInfo {
        sType: info.sType,
        pNext: info.pNext,
        flags: info.flags,
        pApplicationInfo: info.pApplicationInfo,
        enabledLayerCount: info.enabledLayerCount,
        ppEnabledLayerNames: info.ppEnabledLayerNames,
        enabledExtensionCount: ext_ptrs.len() as u32,
        ppEnabledExtensionNames: if ext_ptrs.is_empty() {
            std::ptr::null()
        } else {
            ext_ptrs.as_ptr()
        },
    };
    let res = (h.create_instance)(&patched, pAllocator, pInstance);
    if trace_enabled() {
        eprintln!(
            "[vkshim] vkCreateInstance res={res} instance={:p} exts={:?}",
            *pInstance,
            filtered_exts
                .iter()
                .map(|s| s.to_string_lossy())
                .collect::<Vec<_>>()
        );
    }
    if res == VK_SUCCESS && !(*pInstance).is_null() {
        lock_state().instances.insert(*pInstance as usize);
    }
    res
}

#[no_mangle]
pub unsafe extern "C" fn vkDestroyInstance(instance: VkInstance, pAllocator: *const c_void) {
    if !instance.is_null() {
        if let Ok(h) = host() {
            (h.destroy_instance)(instance, pAllocator);
        }
        lock_state().instances.remove(&(instance as usize));
    }
}

#[no_mangle]
pub unsafe extern "C" fn vkEnumerateDeviceExtensionProperties(
    physicalDevice: VkPhysicalDevice,
    pLayerName: *const c_char,
    pPropertyCount: *mut u32,
    pProperties: *mut VkExtensionProperties,
) -> VkResult {
    let Ok(h) = host() else {
        return VK_ERROR_INITIALIZATION_FAILED;
    };
    let f = h.enum_device_exts;
    if pPropertyCount.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let mut host_count = 0u32;
    let res = f(
        physicalDevice,
        pLayerName,
        &mut host_count,
        std::ptr::null_mut(),
    );
    if res != VK_SUCCESS {
        return res;
    }
    let mut list = vec![
        VkExtensionProperties {
            extensionName: [0; 256],
            specVersion: 0,
        };
        host_count as usize
    ];
    if host_count > 0 {
        let res = f(
            physicalDevice,
            pLayerName,
            &mut host_count,
            list.as_mut_ptr(),
        );
        if res < 0 {
            return res;
        }
        list.truncate(host_count as usize);
    }
    if pLayerName.is_null() && !list.iter().any(|p| ext_name(p) == "VK_KHR_swapchain") {
        let mut prop = VkExtensionProperties {
            extensionName: [0; 256],
            specVersion: 0,
        };
        write_ext_prop(&mut prop, "VK_KHR_swapchain", 70);
        list.push(prop);
    }
    if pProperties.is_null() {
        *pPropertyCount = list.len() as u32;
        return VK_SUCCESS;
    }
    let cap = *pPropertyCount as usize;
    let copy_n = cap.min(list.len());
    std::ptr::copy_nonoverlapping(list.as_ptr(), pProperties, copy_n);
    *pPropertyCount = copy_n as u32;
    if copy_n < list.len() { 5 } else { VK_SUCCESS }
}

#[no_mangle]
pub unsafe extern "C" fn vkCreateDevice(
    physicalDevice: VkPhysicalDevice,
    pCreateInfo: *const VkDeviceCreateInfo,
    pAllocator: *const c_void,
    pDevice: *mut VkDevice,
) -> VkResult {
    let Ok(h) = host() else {
        return VK_ERROR_INITIALIZATION_FAILED;
    };
    let instance = {
        let st = lock_state();
        st.instances.iter().copied().next().unwrap_or(0) as VkInstance
    };
    let host_create_device = h.create_device;
    if pCreateInfo.is_null() || pDevice.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }

    // Query supported host device extensions so we only enable what exists.
    let mut supported: HashSet<String> = HashSet::new();
    {
        let enum_dev = h.enum_device_exts;
        let mut cnt = 0u32;
        if enum_dev(
            physicalDevice,
            std::ptr::null(),
            &mut cnt,
            std::ptr::null_mut(),
        ) == VK_SUCCESS
            && cnt > 0
        {
            let mut props = vec![
                VkExtensionProperties {
                    extensionName: [0; 256],
                    specVersion: 0,
                };
                cnt as usize
            ];
            if enum_dev(
                physicalDevice,
                std::ptr::null(),
                &mut cnt,
                props.as_mut_ptr(),
            ) >= 0
            {
                for p in props.iter().take(cnt as usize) {
                    supported.insert(ext_name(p).to_string());
                }
            }
        }
    }

    let info = &*pCreateInfo;
    let mut exts: Vec<CString> = Vec::new();
    if info.enabledExtensionCount > 0 && !info.ppEnabledExtensionNames.is_null() {
        let slice = std::slice::from_raw_parts(
            info.ppEnabledExtensionNames,
            info.enabledExtensionCount as usize,
        );
        for &ptr in slice {
            if ptr.is_null() {
                continue;
            }
            let name = CStr::from_ptr(ptr).to_string_lossy();
            if name.starts_with("VK_ANDROID_") || name == "VK_GOOGLE_display_timing" {
                continue;
            }
            if !supported.is_empty() && !supported.contains(name.as_ref()) {
                continue;
            }
            if let Ok(cs) = CString::new(name.as_ref()) {
                exts.push(cs);
            }
        }
    }
    for extra in [
        "VK_KHR_external_memory",
        "VK_KHR_external_memory_fd",
        "VK_EXT_external_memory_dma_buf",
        "VK_EXT_image_drm_format_modifier",
        "VK_KHR_image_format_list",
    ] {
        if supported.contains(extra) && !exts.iter().any(|s| s.to_str() == Ok(extra)) {
            exts.push(CString::new(extra).unwrap());
        }
    }
    let ext_ptrs: Vec<*const c_char> = exts.iter().map(|s| s.as_ptr()).collect();
    let patched = VkDeviceCreateInfo {
        sType: info.sType,
        pNext: info.pNext,
        flags: info.flags,
        queueCreateInfoCount: info.queueCreateInfoCount,
        pQueueCreateInfos: info.pQueueCreateInfos,
        enabledLayerCount: info.enabledLayerCount,
        ppEnabledLayerNames: info.ppEnabledLayerNames,
        enabledExtensionCount: ext_ptrs.len() as u32,
        ppEnabledExtensionNames: if ext_ptrs.is_empty() {
            std::ptr::null()
        } else {
            ext_ptrs.as_ptr()
        },
        pEnabledFeatures: info.pEnabledFeatures,
    };

    let res = host_create_device(physicalDevice, &patched, pAllocator, pDevice);
    if res == VK_SUCCESS && !(*pDevice).is_null() {
        let dev = *pDevice;
        let qfam = if info.queueCreateInfoCount > 0 && !info.pQueueCreateInfos.is_null() {
            (*info.pQueueCreateInfos).queueFamilyIndex
        } else {
            0
        };
        let mut queue: VkQueue = std::ptr::null_mut();
        if let Some(gq) = host_device_fn(dev, b"vkGetDeviceQueue\0") {
            let get_queue: PfnGetDeviceQueue = std::mem::transmute(gq);
            get_queue(dev, qfam, 0, &mut queue);
        }
        lock_state().devices.insert(
            dev as usize,
            DeviceRec {
                instance,
                physical_device: physicalDevice,
                drm_modifiers: supported.contains("VK_EXT_image_drm_format_modifier"),
                queue,
            },
        );
        if trace_enabled() {
            eprintln!("[vkshim] vkCreateDevice -> {dev:p} (queue={queue:p})");
        }
    }
    res
}

#[no_mangle]
pub unsafe extern "C" fn vkDestroyDevice(device: VkDevice, pAllocator: *const c_void) {
    if !device.is_null() {
        if let Ok(h) = host() {
            (h.destroy_device)(device, pAllocator);
        }
        lock_state().devices.remove(&(device as usize));
    }
}

#[no_mangle]
pub unsafe extern "C" fn vkCreateAndroidSurfaceKHR(
    _instance: VkInstance,
    pCreateInfo: *const VkAndroidSurfaceCreateInfoKHR,
    _pAllocator: *const c_void,
    pSurface: *mut VkSurfaceKHR,
) -> VkResult {
    if pCreateInfo.is_null() || pSurface.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let window = (*pCreateInfo).window;
    let (w, h) = query_native_window_size(window);
    let mut st = lock_state();
    st.next_handle += 1;
    let handle = 0x564b_5355_0000_0000u64 | st.next_handle;
    let sid = ensure_daemon_surface_id(&mut st, w, h).unwrap_or(0);
    st.surfaces.insert(
        handle,
        SurfaceRec {
            window,
            width: w,
            height: h,
            surface_id: sid,
        },
    );
    *pSurface = handle;
    if trace_enabled() {
        eprintln!(
            "[vkshim] vkCreateAndroidSurfaceKHR window={window:p} -> 0x{handle:x} ({w}x{h}, sid={sid})"
        );
    }
    VK_SUCCESS
}

#[no_mangle]
pub unsafe extern "C" fn vkDestroySurfaceKHR(
    instance: VkInstance,
    surface: VkSurfaceKHR,
    pAllocator: *const c_void,
) {
    if surface == 0 {
        return;
    }
    if lock_state().surfaces.remove(&surface).is_some() {
        return;
    }
    if let Some(f) = host_instance_fn(instance, b"vkDestroySurfaceKHR\0") {
        let f: unsafe extern "C" fn(VkInstance, VkSurfaceKHR, *const c_void) =
            std::mem::transmute(f);
        f(instance, surface, pAllocator);
    }
}

#[no_mangle]
pub unsafe extern "C" fn vkGetPhysicalDeviceSurfaceSupportKHR(
    _physicalDevice: VkPhysicalDevice,
    _queueFamilyIndex: u32,
    surface: VkSurfaceKHR,
    pSupported: *mut VkBool32,
) -> VkResult {
    if pSupported.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    *pSupported = if surface != 0 { VK_TRUE } else { VK_FALSE };
    VK_SUCCESS
}

#[no_mangle]
pub unsafe extern "C" fn vkGetPhysicalDeviceSurfaceCapabilitiesKHR(
    _physicalDevice: VkPhysicalDevice,
    surface: VkSurfaceKHR,
    pSurfaceCapabilities: *mut VkSurfaceCapabilitiesKHR,
) -> VkResult {
    if trace_enabled() {
        eprintln!("[vkshim] surface capabilities query handle=0x{surface:x}");
    }
    if pSurfaceCapabilities.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let rec = lock_state().surfaces.get(&surface).copied();
    let Some(mut rec) = rec else {
        if trace_enabled() {
            eprintln!("[vkshim] surface capabilities: unknown handle");
        }
        return VK_ERROR_SURFACE_LOST_KHR;
    };
    let (w, h) = query_native_window_size(rec.window);
    if w > 0 && h > 0 {
        rec.width = w;
        rec.height = h;
        lock_state().surfaces.insert(surface, rec);
    }
    *pSurfaceCapabilities = VkSurfaceCapabilitiesKHR {
        minImageCount: 2,
        maxImageCount: 8,
        currentExtent: VkExtent2D {
            width: rec.width,
            height: rec.height,
        },
        minImageExtent: VkExtent2D {
            width: 1,
            height: 1,
        },
        maxImageExtent: VkExtent2D {
            width: 16384,
            height: 16384,
        },
        maxImageArrayLayers: 1,
        supportedTransforms: 0x1,     // IDENTITY
        currentTransform: 0x1,        // IDENTITY
        supportedCompositeAlpha: 0x9, // OPAQUE | INHERIT
        supportedUsageFlags: 0x1F, // TRANSFER_SRC | TRANSFER_DST | SAMPLED | STORAGE | COLOR_ATTACHMENT
    };
    VK_SUCCESS
}

#[no_mangle]
pub unsafe extern "C" fn vkGetPhysicalDeviceSurfaceCapabilities2KHR(
    physicalDevice: VkPhysicalDevice,
    pSurfaceInfo: *const c_void,
    pSurfaceCapabilities: *mut VkSurfaceCapabilities2KHR,
) -> VkResult {
    if pSurfaceInfo.is_null() || pSurfaceCapabilities.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    // VkPhysicalDeviceSurfaceInfo2KHR: sType (i32), pad (u32), pNext (*const), surface (u64)
    #[repr(C)]
    struct SurfaceInfo2 {
        s_type: i32,
        p_next: *const c_void,
        surface: VkSurfaceKHR,
    }
    let surface = (*(pSurfaceInfo as *const SurfaceInfo2)).surface;
    vkGetPhysicalDeviceSurfaceCapabilitiesKHR(
        physicalDevice,
        surface,
        &mut (*pSurfaceCapabilities).surfaceCapabilities,
    )
}

#[no_mangle]
pub unsafe extern "C" fn vkGetPhysicalDeviceSurfaceFormatsKHR(
    _physicalDevice: VkPhysicalDevice,
    _surface: VkSurfaceKHR,
    pSurfaceFormatCount: *mut u32,
    pSurfaceFormats: *mut VkSurfaceFormatKHR,
) -> VkResult {
    if trace_enabled() && !pSurfaceFormatCount.is_null() {
        eprintln!(
            "[vkshim] surface formats: capacity={} output={pSurfaceFormats:p}",
            *pSurfaceFormatCount
        );
    }
    if pSurfaceFormatCount.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let formats = [
        VkSurfaceFormatKHR {
            format: VK_FORMAT_B8G8R8A8_UNORM,
            colorSpace: VK_COLOR_SPACE_SRGB_NONLINEAR_KHR,
        },
        VkSurfaceFormatKHR {
            format: VK_FORMAT_R8G8B8A8_UNORM,
            colorSpace: VK_COLOR_SPACE_SRGB_NONLINEAR_KHR,
        },
        VkSurfaceFormatKHR {
            format: VK_FORMAT_B8G8R8A8_SRGB,
            colorSpace: VK_COLOR_SPACE_SRGB_NONLINEAR_KHR,
        },
        VkSurfaceFormatKHR {
            format: VK_FORMAT_R8G8B8A8_SRGB,
            colorSpace: VK_COLOR_SPACE_SRGB_NONLINEAR_KHR,
        },
    ];
    if pSurfaceFormats.is_null() {
        *pSurfaceFormatCount = formats.len() as u32;
        return VK_SUCCESS;
    }
    let cap = *pSurfaceFormatCount as usize;
    let n = cap.min(formats.len());
    std::ptr::copy_nonoverlapping(formats.as_ptr(), pSurfaceFormats, n);
    *pSurfaceFormatCount = n as u32;
    if n < formats.len() { 5 } else { VK_SUCCESS }
}

#[no_mangle]
pub unsafe extern "C" fn vkGetPhysicalDeviceSurfaceFormats2KHR(
    physicalDevice: VkPhysicalDevice,
    pSurfaceInfo: *const c_void,
    pSurfaceFormatCount: *mut u32,
    pSurfaceFormats: *mut VkSurfaceFormat2KHR,
) -> VkResult {
    if pSurfaceFormatCount.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    if pSurfaceFormats.is_null() {
        return vkGetPhysicalDeviceSurfaceFormatsKHR(
            physicalDevice,
            if pSurfaceInfo.is_null() { 1 } else { 1 },
            pSurfaceFormatCount,
            std::ptr::null_mut(),
        );
    }
    let mut tmp = vec![
        VkSurfaceFormatKHR {
            format: 0,
            colorSpace: 0,
        };
        *pSurfaceFormatCount as usize
    ];
    let res = vkGetPhysicalDeviceSurfaceFormatsKHR(
        physicalDevice,
        1,
        pSurfaceFormatCount,
        tmp.as_mut_ptr(),
    );
    if res >= 0 {
        for i in 0..(*pSurfaceFormatCount as usize) {
            (*pSurfaceFormats.add(i)).surfaceFormat = tmp[i];
        }
    }
    res
}

#[no_mangle]
pub unsafe extern "C" fn vkGetPhysicalDeviceSurfacePresentModesKHR(
    _physicalDevice: VkPhysicalDevice,
    _surface: VkSurfaceKHR,
    pPresentModeCount: *mut u32,
    pPresentModes: *mut i32,
) -> VkResult {
    if pPresentModeCount.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let modes = [VK_PRESENT_MODE_FIFO_KHR, VK_PRESENT_MODE_IMMEDIATE_KHR];
    if pPresentModes.is_null() {
        *pPresentModeCount = modes.len() as u32;
        return VK_SUCCESS;
    }
    let cap = *pPresentModeCount as usize;
    let n = cap.min(modes.len());
    std::ptr::copy_nonoverlapping(modes.as_ptr(), pPresentModes, n);
    *pPresentModeCount = n as u32;
    if n < modes.len() { 5 } else { VK_SUCCESS }
}

unsafe fn find_memory_type(
    instance: VkInstance,
    physical_device: VkPhysicalDevice,
    type_bits: u32,
    preferred: u32,
) -> Option<u32> {
    let fp = host_instance_fn(instance, b"vkGetPhysicalDeviceMemoryProperties\0")?;
    let get_mem_props: PfnGetPhysicalDeviceMemoryProperties = std::mem::transmute(fp);
    let mut props = VkPhysicalDeviceMemoryProperties {
        memoryTypeCount: 0,
        memoryTypes: [VkMemoryType::default(); 32],
        memoryHeapCount: 0,
        memoryHeaps: [VkMemoryHeap::default(); 16],
    };
    get_mem_props(physical_device, &mut props);
    for i in 0..props.memoryTypeCount {
        if (type_bits & (1 << i)) != 0
            && (props.memoryTypes[i as usize].propertyFlags & preferred) == preferred
        {
            return Some(i);
        }
    }
    for i in 0..props.memoryTypeCount {
        if (type_bits & (1 << i)) != 0 {
            return Some(i);
        }
    }
    None
}

#[no_mangle]
pub unsafe extern "C" fn vkCreateSwapchainKHR(
    device: VkDevice,
    pCreateInfo: *const VkSwapchainCreateInfoKHR,
    _pAllocator: *const c_void,
    pSwapchain: *mut VkSwapchainKHR,
) -> VkResult {
    if pCreateInfo.is_null() || pSwapchain.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let info = &*pCreateInfo;
    if trace_enabled() {
        eprintln!(
            "[vkshim] create swapchain device={device:p} extent={}x{} format={} usage=0x{:x}",
            info.imageExtent.width, info.imageExtent.height, info.imageFormat, info.imageUsage
        );
    }
    let (instance, physical_device, drm_modifiers) = {
        let st = lock_state();
        match st.devices.get(&(device as usize)) {
            Some(d) => (d.instance, d.physical_device, d.drm_modifiers),
            None => return VK_ERROR_DEVICE_LOST,
        }
    };

    macro_rules! dev_fn {
        ($sym:literal, $ty:ty) => {{
            let Some(p) = host_device_fn(device, concat!($sym, "\0").as_bytes()) else {
                return VK_ERROR_INITIALIZATION_FAILED;
            };
            std::mem::transmute::<*mut c_void, $ty>(p)
        }};
    }
    let create_image = dev_fn!("vkCreateImage", PfnCreateImage);
    let destroy_image = dev_fn!("vkDestroyImage", PfnDestroyImage);
    let get_img_reqs = dev_fn!(
        "vkGetImageMemoryRequirements",
        PfnGetImageMemoryRequirements
    );
    let alloc_mem = dev_fn!("vkAllocateMemory", PfnAllocateMemory);
    let free_mem = dev_fn!("vkFreeMemory", PfnFreeMemory);
    let bind_img_mem = dev_fn!("vkBindImageMemory", PfnBindImageMemory);
    let get_sub_layout = dev_fn!("vkGetImageSubresourceLayout", PfnGetImageSubresourceLayout);
    let get_mem_fd = dev_fn!("vkGetMemoryFdKHR", PfnGetMemoryFdKHR);

    let count = info.minImageCount.clamp(2, 4) as usize;
    let width = info.imageExtent.width.max(1);
    let height = info.imageExtent.height.max(1);
    let fourcc = format_to_fourcc(info.imageFormat);
    let mut images: Vec<SwapchainImage> = Vec::with_capacity(count);
    let modifier = if drm_modifiers {
        exportable_drm_modifier(instance, physical_device, info.imageFormat, info.imageUsage)
    } else {
        None
    };
    if trace_enabled() {
        eprintln!("[vkshim] swapchain DRM modifier: {modifier:?}");
    }

    for _ in 0..count {
        let selected_modifier = modifier.unwrap_or(0);
        let drm_list = DrmImageList {
            s_type: 1000158003,
            next: std::ptr::null(),
            count: 1,
            modifiers: &selected_modifier,
        };
        let ext_img = VkExternalMemoryImageCreateInfo {
            sType: VK_STRUCTURE_TYPE_EXTERNAL_MEMORY_IMAGE_CREATE_INFO,
            pNext: if modifier.is_some() {
                (&drm_list as *const DrmImageList).cast()
            } else {
                std::ptr::null()
            },
            handleTypes: VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
        };
        let img_info = VkImageCreateInfo {
            sType: VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO,
            pNext: &ext_img as *const _ as *const c_void,
            flags: 0,
            imageType: VK_IMAGE_TYPE_2D,
            format: info.imageFormat,
            extent: VkExtent3D {
                width,
                height,
                depth: 1,
            },
            mipLevels: 1,
            arrayLayers: info.imageArrayLayers.max(1),
            samples: VK_SAMPLE_COUNT_1_BIT,
            tiling: if modifier.is_some() {
                1000158000
            } else {
                VK_IMAGE_TILING_LINEAR
            },
            usage: info.imageUsage,
            sharingMode: VK_SHARING_MODE_EXCLUSIVE,
            queueFamilyIndexCount: 0,
            pQueueFamilyIndices: std::ptr::null(),
            initialLayout: VK_IMAGE_LAYOUT_UNDEFINED,
        };
        let mut image: VkImage = 0;
        let r = create_image(device, &img_info, std::ptr::null(), &mut image);
        if r != VK_SUCCESS {
            eprintln!("[vkshim] swapchain vkCreateImage failed: {r}");
            for prev in images {
                destroy_image(device, prev.image, std::ptr::null());
                free_mem(device, prev.memory, std::ptr::null());
            }
            return r;
        }
        let mut reqs = VkMemoryRequirements::default();
        get_img_reqs(device, image, &mut reqs);
        let Some(mem_type) = find_memory_type(
            instance,
            physical_device,
            reqs.memoryTypeBits,
            VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT,
        )
        .or_else(|| {
            find_memory_type(
                instance,
                physical_device,
                reqs.memoryTypeBits,
                VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT,
            )
        }) else {
            destroy_image(device, image, std::ptr::null());
            return VK_ERROR_OUT_OF_HOST_MEMORY;
        };

        let dedicated = VkMemoryDedicatedAllocateInfo {
            sType: VK_STRUCTURE_TYPE_MEMORY_DEDICATED_ALLOCATE_INFO,
            pNext: std::ptr::null(),
            image,
            buffer: 0,
        };
        let export_info = VkExportMemoryAllocateInfo {
            sType: VK_STRUCTURE_TYPE_EXPORT_MEMORY_ALLOCATE_INFO,
            pNext: &dedicated as *const _ as *const c_void,
            handleTypes: VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
        };
        let alloc_info = VkMemoryAllocateInfo {
            sType: VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
            pNext: &export_info as *const _ as *const c_void,
            allocationSize: reqs.size,
            memoryTypeIndex: mem_type,
        };
        let mut memory: VkDeviceMemory = 0;
        let r = alloc_mem(device, &alloc_info, std::ptr::null(), &mut memory);
        if r != VK_SUCCESS {
            eprintln!("[vkshim] swapchain vkAllocateMemory failed: {r}");
            destroy_image(device, image, std::ptr::null());
            return r;
        }
        let r = bind_img_mem(device, image, memory, 0);
        if r != VK_SUCCESS {
            eprintln!("[vkshim] swapchain vkBindImageMemory failed: {r}");
            destroy_image(device, image, std::ptr::null());
            free_mem(device, memory, std::ptr::null());
            return r;
        }

        let sub = VkImageSubresource {
            aspectMask: if modifier.is_some() {
                0x80
            } else {
                VK_IMAGE_ASPECT_COLOR_BIT
            },
            mipLevel: 0,
            arrayLayer: 0,
        };
        let mut layout = VkSubresourceLayout::default();
        get_sub_layout(device, image, &sub, &mut layout);

        let fd_info = VkMemoryGetFdInfoKHR {
            sType: VK_STRUCTURE_TYPE_MEMORY_GET_FD_INFO_KHR,
            pNext: std::ptr::null(),
            memory,
            handleType: VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT,
        };
        let mut raw_fd: c_int = -1;
        let r = get_mem_fd(device, &fd_info, &mut raw_fd);
        if r != VK_SUCCESS || raw_fd < 0 {
            eprintln!("[vkshim] swapchain vkGetMemoryFdKHR failed: {r}, fd={raw_fd}");
            destroy_image(device, image, std::ptr::null());
            free_mem(device, memory, std::ptr::null());
            return VK_ERROR_INITIALIZATION_FAILED;
        }

        images.push(SwapchainImage {
            image,
            memory,
            dmabuf_fd: OwnedFd::from_raw_fd(raw_fd),
            stride: layout.rowPitch as u32,
            offset: layout.offset as u32,
            fourcc,
            modifier: modifier.unwrap_or(DRM_FORMAT_MOD_LINEAR),
        });
    }

    let mut st = lock_state();
    st.next_handle += 1;
    let handle = 0x564b_5357_0000_0000u64 | st.next_handle;
    st.swapchains.insert(
        handle,
        SwapchainRec {
            device,
            surface: info.surface,
            width,
            height,
            images,
            next_index: 0,
        },
    );
    *pSwapchain = handle;
    if trace_enabled() {
        eprintln!(
            "[vkshim] vkCreateSwapchainKHR -> 0x{handle:x} ({width}x{height}, {count} images)"
        );
    }
    VK_SUCCESS
}

#[no_mangle]
pub unsafe extern "C" fn vkDestroySwapchainKHR(
    device: VkDevice,
    swapchain: VkSwapchainKHR,
    _pAllocator: *const c_void,
) {
    if swapchain == 0 {
        return;
    }
    let removed = lock_state().swapchains.remove(&swapchain);
    if let Some(sc) = removed {
        if let (Some(di), Some(fm)) = (
            host_device_fn(device, b"vkDestroyImage\0"),
            host_device_fn(device, b"vkFreeMemory\0"),
        ) {
            let destroy_image: PfnDestroyImage = std::mem::transmute(di);
            let free_mem: PfnFreeMemory = std::mem::transmute(fm);
            for img in sc.images {
                destroy_image(device, img.image, std::ptr::null());
                free_mem(device, img.memory, std::ptr::null());
            }
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn vkGetSwapchainImagesKHR(
    _device: VkDevice,
    swapchain: VkSwapchainKHR,
    pSwapchainImageCount: *mut u32,
    pSwapchainImages: *mut VkImage,
) -> VkResult {
    if pSwapchainImageCount.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let st = lock_state();
    let Some(sc) = st.swapchains.get(&swapchain) else {
        return VK_ERROR_SURFACE_LOST_KHR;
    };
    if pSwapchainImages.is_null() {
        *pSwapchainImageCount = sc.images.len() as u32;
        return VK_SUCCESS;
    }
    let cap = *pSwapchainImageCount as usize;
    let n = cap.min(sc.images.len());
    for (i, img) in sc.images.iter().take(n).enumerate() {
        *pSwapchainImages.add(i) = img.image;
    }
    *pSwapchainImageCount = n as u32;
    if n < sc.images.len() { 5 } else { VK_SUCCESS }
}

#[no_mangle]
pub unsafe extern "C" fn vkAcquireNextImageKHR(
    device: VkDevice,
    swapchain: VkSwapchainKHR,
    _timeout: u64,
    semaphore: VkSemaphore,
    fence: VkFence,
    pImageIndex: *mut u32,
) -> VkResult {
    if pImageIndex.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let (idx, queue) = {
        let mut st = lock_state();
        let queue = st
            .devices
            .get(&(device as usize))
            .map(|d| d.queue)
            .unwrap_or(std::ptr::null_mut());
        let Some(sc) = st.swapchains.get_mut(&swapchain) else {
            return VK_ERROR_SURFACE_LOST_KHR;
        };
        if sc.images.is_empty() {
            return VK_NOT_READY;
        }
        let idx = sc.next_index % sc.images.len();
        sc.next_index = sc.next_index.wrapping_add(1);
        (idx as u32, queue)
    };
    *pImageIndex = idx;

    if (semaphore != 0 || fence != 0) && !queue.is_null() {
        if let Some(qs) = host_device_fn(device, b"vkQueueSubmit\0") {
            let queue_submit: PfnQueueSubmit = std::mem::transmute(qs);
            let sems = [semaphore];
            let submit = VkSubmitInfo {
                sType: VK_STRUCTURE_TYPE_SUBMIT_INFO,
                pNext: std::ptr::null(),
                waitSemaphoreCount: 0,
                pWaitSemaphores: std::ptr::null(),
                pWaitDstStageMask: std::ptr::null(),
                commandBufferCount: 0,
                pCommandBuffers: std::ptr::null(),
                signalSemaphoreCount: if semaphore != 0 { 1 } else { 0 },
                pSignalSemaphores: if semaphore != 0 {
                    sems.as_ptr()
                } else {
                    std::ptr::null()
                },
            };
            let _ = queue_submit(queue, 1, &submit, fence);
        }
    }
    VK_SUCCESS
}

#[no_mangle]
pub unsafe extern "C" fn vkAcquireNextImage2KHR(
    device: VkDevice,
    pAcquireInfo: *const VkAcquireNextImageInfoKHR,
    pImageIndex: *mut u32,
) -> VkResult {
    if pAcquireInfo.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let info = &*pAcquireInfo;
    vkAcquireNextImageKHR(
        device,
        info.swapchain,
        info.timeout,
        info.semaphore,
        info.fence,
        pImageIndex,
    )
}

#[no_mangle]
pub unsafe extern "C" fn vkQueuePresentKHR(
    queue: VkQueue,
    pPresentInfo: *const VkPresentInfoKHR,
) -> VkResult {
    if pPresentInfo.is_null() {
        return VK_ERROR_INITIALIZATION_FAILED;
    }
    let info = &*pPresentInfo;

    // Ensure GPU rendering into the presented image(s) has completed.
    if let Some(device) = {
        let st = lock_state();
        info.swapchainCount.checked_sub(1).and_then(|_| {
            if !info.pSwapchains.is_null() {
                st.swapchains.get(&*info.pSwapchains).map(|s| s.device)
            } else {
                None
            }
        })
    } {
        if info.waitSemaphoreCount > 0 && !info.pWaitSemaphores.is_null() {
            if let Some(qs) = host_device_fn(device, b"vkQueueSubmit\0") {
                let queue_submit: PfnQueueSubmit = std::mem::transmute(qs);
                let stages =
                    vec![VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT; info.waitSemaphoreCount as usize];
                let submit = VkSubmitInfo {
                    sType: VK_STRUCTURE_TYPE_SUBMIT_INFO,
                    pNext: std::ptr::null(),
                    waitSemaphoreCount: info.waitSemaphoreCount,
                    pWaitSemaphores: info.pWaitSemaphores,
                    pWaitDstStageMask: stages.as_ptr(),
                    commandBufferCount: 0,
                    pCommandBuffers: std::ptr::null(),
                    signalSemaphoreCount: 0,
                    pSignalSemaphores: std::ptr::null(),
                };
                let _ = queue_submit(queue, 1, &submit, 0);
            }
        }
        if let Some(qwi) = host_device_fn(device, b"vkQueueWaitIdle\0") {
            let wait_idle: PfnQueueWaitIdle = std::mem::transmute(qwi);
            let _ = wait_idle(queue);
        } else if let Some(dwi) = host_device_fn(device, b"vkDeviceWaitIdle\0") {
            let wait_idle: PfnDeviceWaitIdle = std::mem::transmute(dwi);
            let _ = wait_idle(device);
        }
    }

    let mut overall = VK_SUCCESS;
    for i in 0..(info.swapchainCount as usize) {
        let sc_handle = *info.pSwapchains.add(i);
        let img_idx = *info.pImageIndices.add(i) as usize;
        let present_params = {
            let st = lock_state();
            let Some(sc) = st.swapchains.get(&sc_handle) else {
                if !info.pResults.is_null() {
                    *info.pResults.add(i) = VK_ERROR_SURFACE_LOST_KHR;
                }
                overall = VK_ERROR_SURFACE_LOST_KHR;
                continue;
            };
            let Some(img) = sc.images.get(img_idx) else {
                if !info.pResults.is_null() {
                    *info.pResults.add(i) = VK_SUBOPTIMAL_KHR;
                }
                continue;
            };
            let sid = st
                .surfaces
                .get(&sc.surface)
                .map(|s| s.surface_id)
                .unwrap_or(0);
            let dup_fd = libc::dup(img.dmabuf_fd.as_raw_fd());
            (
                sid,
                sc.width,
                sc.height,
                img.stride,
                img.fourcc,
                img.modifier,
                img.offset,
                dup_fd,
            )
        };
        let (sid, w, h, stride, fourcc, modifier, offset, dup_fd) = present_params;
        if dup_fd >= 0 {
            let res = present_dmabuf_to_daemon(sid, w, h, stride, fourcc, modifier, offset, dup_fd);
            libc::close(dup_fd);
            if let Err(e) = res {
                if trace_enabled() {
                    eprintln!("[vkshim] vkQueuePresentKHR failed: {e}");
                }
            } else if trace_enabled() {
                eprintln!("[vkshim] vkQueuePresentKHR presented {w}x{h} idx={img_idx}");
            }
        }
        if !info.pResults.is_null() {
            *info.pResults.add(i) = VK_SUCCESS;
        }
    }
    overall
}

#[no_mangle]
pub unsafe extern "C" fn vkGetInstanceProcAddr(
    instance: VkInstance,
    pName: *const c_char,
) -> PFN_vkVoidFunction {
    if pName.is_null() {
        return None;
    }
    let name = CStr::from_ptr(pName).to_str().unwrap_or("");
    if trace_enabled() {
        eprintln!("[vkshim] vkGetInstanceProcAddr({instance:p}, {name})");
    }
    let intercepted: *const c_void = match name {
        "vkGetInstanceProcAddr" => vkGetInstanceProcAddr as *const c_void,
        "vkGetDeviceProcAddr" => vkGetDeviceProcAddr as *const c_void,
        "vkEnumerateInstanceExtensionProperties" => {
            vkEnumerateInstanceExtensionProperties as *const c_void
        }
        "vkEnumerateDeviceExtensionProperties" => {
            vkEnumerateDeviceExtensionProperties as *const c_void
        }
        "vkCreateInstance" => vkCreateInstance as *const c_void,
        "vkDestroyInstance" => vkDestroyInstance as *const c_void,
        "vkCreateDevice" => vkCreateDevice as *const c_void,
        "vkDestroyDevice" => vkDestroyDevice as *const c_void,
        "vkCreateAndroidSurfaceKHR" => vkCreateAndroidSurfaceKHR as *const c_void,
        "vkDestroySurfaceKHR" => vkDestroySurfaceKHR as *const c_void,
        "vkGetPhysicalDeviceSurfaceSupportKHR" => {
            vkGetPhysicalDeviceSurfaceSupportKHR as *const c_void
        }
        "vkGetPhysicalDeviceSurfaceCapabilitiesKHR" => {
            vkGetPhysicalDeviceSurfaceCapabilitiesKHR as *const c_void
        }
        "vkGetPhysicalDeviceSurfaceCapabilities2KHR" => {
            vkGetPhysicalDeviceSurfaceCapabilities2KHR as *const c_void
        }
        "vkGetPhysicalDeviceSurfaceFormatsKHR" => {
            vkGetPhysicalDeviceSurfaceFormatsKHR as *const c_void
        }
        "vkGetPhysicalDeviceSurfaceFormats2KHR" => {
            vkGetPhysicalDeviceSurfaceFormats2KHR as *const c_void
        }
        "vkGetPhysicalDeviceSurfacePresentModesKHR" => {
            vkGetPhysicalDeviceSurfacePresentModesKHR as *const c_void
        }
        "vkCreateSwapchainKHR" => vkCreateSwapchainKHR as *const c_void,
        "vkDestroySwapchainKHR" => vkDestroySwapchainKHR as *const c_void,
        "vkGetSwapchainImagesKHR" => vkGetSwapchainImagesKHR as *const c_void,
        "vkAcquireNextImageKHR" => vkAcquireNextImageKHR as *const c_void,
        "vkAcquireNextImage2KHR" => vkAcquireNextImage2KHR as *const c_void,
        "vkQueuePresentKHR" => vkQueuePresentKHR as *const c_void,
        _ => std::ptr::null(),
    };
    if !intercepted.is_null() {
        if trace_enabled()
            && (name == "vkCreateInstance"
                || name == "vkCreateAndroidSurfaceKHR"
                || name == "vkCreateSwapchainKHR"
                || name == "vkEnumerateInstanceExtensionProperties")
        {
            eprintln!("[vkshim] vkGetInstanceProcAddr({instance:p}, {name}) -> intercepted");
        }
        return Some(std::mem::transmute::<*const c_void, unsafe extern "C" fn()>(intercepted));
    }
    let Ok(h) = host() else {
        return None;
    };
    let f = (h.get_instance_proc_addr)(instance, pName);
    if trace_enabled() && f.is_none() {
        eprintln!("[vkshim] vkGetInstanceProcAddr({instance:p}, {name}) -> NULL");
    }
    f
}

#[no_mangle]
pub unsafe extern "C" fn vkGetDeviceProcAddr(
    device: VkDevice,
    pName: *const c_char,
) -> PFN_vkVoidFunction {
    if pName.is_null() {
        return None;
    }
    let name = CStr::from_ptr(pName).to_str().unwrap_or("");
    let intercepted: *const c_void = match name {
        "vkGetDeviceProcAddr" => vkGetDeviceProcAddr as *const c_void,
        "vkDestroyDevice" => vkDestroyDevice as *const c_void,
        "vkCreateSwapchainKHR" => vkCreateSwapchainKHR as *const c_void,
        "vkDestroySwapchainKHR" => vkDestroySwapchainKHR as *const c_void,
        "vkGetSwapchainImagesKHR" => vkGetSwapchainImagesKHR as *const c_void,
        "vkAcquireNextImageKHR" => vkAcquireNextImageKHR as *const c_void,
        "vkAcquireNextImage2KHR" => vkAcquireNextImage2KHR as *const c_void,
        "vkQueuePresentKHR" => vkQueuePresentKHR as *const c_void,
        _ => std::ptr::null(),
    };
    if !intercepted.is_null() {
        return Some(std::mem::transmute::<*const c_void, unsafe extern "C" fn()>(intercepted));
    }
    let Ok(h) = host() else {
        return None;
    };
    (h.get_device_proc_addr)(device, pName)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Requires a real Vulkan GPU with DMA-BUF export. This exercises the same
    /// SRGB color-attachment allocation that returned -11 on the RTX 3080.
    #[test]
    #[ignore = "requires a Vulkan GPU with external DMA-BUF support"]
    fn gpu_srgb_swapchain_exports_color_attachments() {
        unsafe {
            let extension = c"VK_KHR_get_physical_device_properties2".as_ptr();
            let mut info: VkInstanceCreateInfo = std::mem::zeroed();
            info.sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO;
            info.enabledExtensionCount = 1;
            info.ppEnabledExtensionNames = &extension;
            let mut instance = std::ptr::null_mut();
            assert_eq!(
                vkCreateInstance(&info, std::ptr::null(), &mut instance),
                VK_SUCCESS
            );
            type Enumerate =
                unsafe extern "C" fn(VkInstance, *mut u32, *mut VkPhysicalDevice) -> VkResult;
            let enumerate: Enumerate = std::mem::transmute(
                host_instance_fn(instance, b"vkEnumeratePhysicalDevices\0").unwrap(),
            );
            let mut count = 0;
            assert_eq!(
                enumerate(instance, &mut count, std::ptr::null_mut()),
                VK_SUCCESS
            );
            assert!(count > 0);
            let mut physical = vec![std::ptr::null_mut(); count as usize];
            assert_eq!(
                enumerate(instance, &mut count, physical.as_mut_ptr()),
                VK_SUCCESS
            );
            type GetQueues = unsafe extern "C" fn(VkPhysicalDevice, *mut u32, *mut [u32; 6]);
            let get_queues: GetQueues = std::mem::transmute(
                host_instance_fn(instance, b"vkGetPhysicalDeviceQueueFamilyProperties\0").unwrap(),
            );
            count = 0;
            get_queues(physical[0], &mut count, std::ptr::null_mut());
            let mut queues = vec![[0; 6]; count as usize];
            get_queues(physical[0], &mut count, queues.as_mut_ptr());
            let family = queues
                .iter()
                .position(|q| q[0] & 1 != 0 && q[1] > 0)
                .unwrap() as u32;
            let priority = 1.0;
            let queue = VkDeviceQueueCreateInfo {
                sType: 2,
                pNext: std::ptr::null(),
                flags: 0,
                queueFamilyIndex: family,
                queueCount: 1,
                pQueuePriorities: &priority,
            };
            let mut info: VkDeviceCreateInfo = std::mem::zeroed();
            info.sType = 3;
            info.queueCreateInfoCount = 1;
            info.pQueueCreateInfos = &queue;
            let mut device = std::ptr::null_mut();
            assert_eq!(
                vkCreateDevice(physical[0], &info, std::ptr::null(), &mut device),
                VK_SUCCESS
            );
            let mut info: VkSwapchainCreateInfoKHR = std::mem::zeroed();
            info.sType = 1000001000;
            info.minImageCount = 3;
            info.imageFormat = 43; // VK_FORMAT_R8G8B8A8_SRGB
            info.imageExtent = VkExtent2D {
                width: 64,
                height: 64,
            };
            info.imageArrayLayers = 1;
            info.imageUsage = 0x93; // ZZZ's transfer/src/dst + color + input usage
            let mut swapchain = 0;
            let result = vkCreateSwapchainKHR(device, &info, std::ptr::null(), &mut swapchain);
            if result == VK_SUCCESS {
                let state = lock_state();
                let images = &state.swapchains.get(&swapchain).unwrap().images;
                assert_eq!(images.len(), 3);
                for image in images {
                    assert!(image.dmabuf_fd.as_raw_fd() >= 0);
                    assert!(image.stride >= 64 * 4);
                }
                drop(state);
                vkDestroySwapchainKHR(device, swapchain, std::ptr::null());
            }
            vkDestroyDevice(device, std::ptr::null());
            vkDestroyInstance(instance, std::ptr::null());
            assert_eq!(
                result, VK_SUCCESS,
                "SRGB renderable DMA-BUF allocation failed"
            );
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn vkGetAndroidHardwareBufferPropertiesANDROID(
    _device: VkDevice,
    _buffer: *const c_void,
    _pProperties: *mut c_void,
) -> VkResult {
    -8 // VK_ERROR_FEATURE_NOT_PRESENT
}

#[no_mangle]
pub unsafe extern "C" fn vkGetMemoryAndroidHardwareBufferANDROID(
    _device: VkDevice,
    _pInfo: *const c_void,
    _pBuffer: *mut *mut c_void,
) -> VkResult {
    -8 // VK_ERROR_FEATURE_NOT_PRESENT
}
