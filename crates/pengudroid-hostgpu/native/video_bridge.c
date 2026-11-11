/* SPDX-License-Identifier: Apache-2.0
 * Host decoder bridge, compiled against public FFmpeg headers plus libva and/or
 * Vulkan, whichever the build host provides (PD_VIDEO_VAAPI / PD_VIDEO_VULKAN).
 * https://ffmpeg.org/doxygen/7.1/group__lavc__decoding.html
 * https://intel.github.io/libva/group__api__core.html
 *
 * VA-API exports the decode surface itself (NV12/P010, no copy). The Vulkan
 * Video backend exists for drivers without VA-API (NVIDIA). Measured on driver
 * 595.99 with Mutter 50:
 *   - decode images cannot be exported (no DRM-modifier tiling for video usage);
 *   - FFmpeg's own Vulkan->DRM mapping crashes inside the driver;
 *   - a YUV dma-buf cannot be presented at all: Mutter imports each plane as
 *     its own EGLImage (R8 + GR88) and NVIDIA's EGL has no GR88, while a linear
 *     two-plane NV12 image cannot be bound as a texture.
 * So that backend converts each frame to RGBA in one compute pass and exports
 * the result, which is the format the same driver already scans out for the
 * app's own GL frames. Neither path maps pixels on the CPU.
 */
#include "video_bridge.h"
#include <libavcodec/avcodec.h>
#include <libavutil/hwcontext.h>
#include <libavutil/mem.h>
#include <libavutil/pixdesc.h>
#ifdef PD_VIDEO_VAAPI
#include <libavutil/hwcontext_vaapi.h>
#include <va/va_drmcommon.h>
#endif
#ifdef PD_VIDEO_VULKAN
#include <libavutil/hwcontext_vulkan.h>
#include "video_convert.spv.h"
#endif
#if !defined(PD_VIDEO_VAAPI) && !defined(PD_VIDEO_VULKAN)
#error "build with -DPD_VIDEO_VAAPI and/or -DPD_VIDEO_VULKAN"
#endif
#include <stdatomic.h>
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

struct slots { atomic_uint refs, held; };
enum backend { BACKEND_VAAPI = 1, BACKEND_VULKAN = 2 };
struct pd_video_decoder {
    AVCodecContext *codec;
    struct slots *slots;
    enum backend backend;
#ifdef PD_VIDEO_VULKAN
    AVBufferRef *device;        /* Keeps the VkDevice alive for the objects below. */
    AVBufferRef *export_frames; /* RGBA, dma-buf-exportable conversion targets. */
    uint64_t export_modifier;
    VkQueue queue;
    uint32_t queue_family;
    VkCommandPool pool;
    VkSampler sampler;
    VkDescriptorSetLayout set_layout;
    VkDescriptorPool descriptors;
    VkDescriptorSet set;
    VkPipelineLayout pipeline_layout;
    VkShaderModule shader;
    VkPipeline pipeline;
#endif
};
struct pd_video_frame {
    /* VA-API: the decode surface. Vulkan: the converted RGBA image. Either way
     * it is retained until the consumer releases the frame. */
    AVFrame *frame;
    enum backend backend;
    int fds[PD_VIDEO_MAX_OBJECTS]; /* Exported dma-buf fds this frame closes. */
    uint32_t fd_count;
    struct slots *slots;
};
static int fail(char *error, uint32_t size, const char *operation, int code) {
    char text[AV_ERROR_MAX_STRING_SIZE];
    av_strerror(code, text, sizeof(text));
    if (error && size) snprintf(error, size, "%s: %s (%d)", operation, text, code);
    return code;
}
static void unref_slots(struct slots *slots) {
    if (atomic_fetch_sub(&slots->refs, 1) == 1) free(slots);
}
uint32_t pd_video_abi(void) { return PD_VIDEO_ABI; }
static enum AVPixelFormat hardware_only(AVCodecContext *context, const enum AVPixelFormat *formats) {
    const struct pd_video_decoder *decoder = context->opaque;
    enum AVPixelFormat wanted = decoder->backend == BACKEND_VAAPI ? AV_PIX_FMT_VAAPI : AV_PIX_FMT_VULKAN;
    for (; *formats != AV_PIX_FMT_NONE; formats++)
        if (*formats == wanted) return *formats;
    return AV_PIX_FMT_NONE;
}
static int codec_supports(const AVCodec *codec, enum AVHWDeviceType type) {
    for (int i = 0;; i++) {
        const AVCodecHWConfig *config = avcodec_get_hw_config(codec, i);
        if (!config) return 0;
        if (config->device_type == type &&
            (config->methods & AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX)) return 1;
    }
}

#ifdef PD_VIDEO_VAAPI
static int export_vaapi(struct pd_video_frame *frame, struct pd_video_frame_info *info, const char **stage) {
    *stage = "export synchronized VA-API frame";
    if (frame->frame->format != AV_PIX_FMT_VAAPI || !frame->frame->hw_frames_ctx) return AVERROR(ENOSYS);
    AVHWFramesContext *frames = (AVHWFramesContext *)frame->frame->hw_frames_ctx->data;
    AVVAAPIDeviceContext *device = (AVVAAPIDeviceContext *)frames->device_ctx->hwctx;
    VASurfaceID surface = (VASurfaceID)(uintptr_t)frame->frame->data[3];
    VADRMPRIMESurfaceDescriptor drm;
    if (vaSyncSurface(device->display, surface) != VA_STATUS_SUCCESS) return AVERROR(EIO);
    /* Composed layers retain NV12/P010 and all planes/modifiers. No CPU map. */
    if (vaExportSurfaceHandle(device->display, surface, VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2,
        VA_EXPORT_SURFACE_READ_ONLY | VA_EXPORT_SURFACE_COMPOSED_LAYERS, &drm) != VA_STATUS_SUCCESS)
        return AVERROR(ENOSYS);
    /* The frame owns every exported fd from here on, including on failure. */
    for (uint32_t i = 0; i < drm.num_objects; i++) {
        if (i < PD_VIDEO_MAX_OBJECTS) frame->fds[frame->fd_count++] = drm.objects[i].fd;
        else close(drm.objects[i].fd);
    }
    if (drm.num_objects > PD_VIDEO_MAX_OBJECTS || drm.num_objects == 0 ||
        drm.num_layers != 1 || drm.layers[0].num_planes > PD_VIDEO_MAX_PLANES ||
        drm.layers[0].num_planes == 0) return AVERROR(ENOSYS);
    info->fourcc = drm.layers[0].drm_format;
    info->object_count = drm.num_objects;
    info->plane_count = drm.layers[0].num_planes;
    for (uint32_t i = 0; i < info->object_count; i++)
        info->objects[i] = (struct pd_video_object) {drm.objects[i].fd,
            drm.objects[i].size, drm.objects[i].drm_format_modifier};
    for (uint32_t i = 0; i < info->plane_count; i++) {
        if (drm.layers[0].object_index[i] >= info->object_count) return AVERROR(EINVAL);
        info->planes[i] = (struct pd_video_plane) {drm.layers[0].object_index[i],
            drm.layers[0].offset[i], drm.layers[0].pitch[i]};
    }
    return 0;
}
#endif

#ifdef PD_VIDEO_VULKAN
/* Push constants of video_convert.comp. */
struct convert_parameters { float range[4]; float matrix[4]; };
/* Select the Vulkan device that owns this DRM render node, not "device 0",
 * then build the one compute pipeline every frame is converted with. Anything
 * created before a failure is destroyed by pd_video_close(). */
static int vulkan_device(struct pd_video_decoder *decoder, const char *node) {
    AVBufferRef *drm = NULL;
    int rc = av_hwdevice_ctx_create(&drm, AV_HWDEVICE_TYPE_DRM, node, NULL, 0);
    if (rc < 0) return rc;
    rc = av_hwdevice_ctx_create_derived(&decoder->codec->hw_device_ctx, AV_HWDEVICE_TYPE_VULKAN, drm, 0);
    av_buffer_unref(&drm);
    if (rc < 0) return rc;
    decoder->device = av_buffer_ref(decoder->codec->hw_device_ctx);
    if (!decoder->device) return AVERROR(ENOMEM);
    AVVulkanDeviceContext *vk = ((AVHWDeviceContext *)decoder->device->data)->hwctx;
    int family = -1;
    for (int i = 0; i < vk->nb_qf && family < 0; i++)
        if (vk->qf[i].num > 0 && (vk->qf[i].flags & VK_QUEUE_COMPUTE_BIT)) family = vk->qf[i].idx;
    if (family < 0) return AVERROR(ENOSYS);
    decoder->queue_family = (uint32_t)family;
    vkGetDeviceQueue(vk->act_dev, decoder->queue_family, 0, &decoder->queue);
    VkCommandPoolCreateInfo pool = {.sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
        .flags = VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT, .queueFamilyIndex = decoder->queue_family};
    VkSamplerCreateInfo sampler = {.sType = VK_STRUCTURE_TYPE_SAMPLER_CREATE_INFO,
        .magFilter = VK_FILTER_LINEAR, .minFilter = VK_FILTER_LINEAR, .mipmapMode = VK_SAMPLER_MIPMAP_MODE_NEAREST,
        .addressModeU = VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE, .addressModeV = VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE,
        .addressModeW = VK_SAMPLER_ADDRESS_MODE_CLAMP_TO_EDGE};
    VkDescriptorSetLayoutBinding bindings[3] = {
        {0, VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER, 1, VK_SHADER_STAGE_COMPUTE_BIT, NULL},
        {1, VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER, 1, VK_SHADER_STAGE_COMPUTE_BIT, NULL},
        {2, VK_DESCRIPTOR_TYPE_STORAGE_IMAGE, 1, VK_SHADER_STAGE_COMPUTE_BIT, NULL}};
    VkDescriptorSetLayoutCreateInfo set_layout = {.sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_LAYOUT_CREATE_INFO,
        .bindingCount = 3, .pBindings = bindings};
    VkDescriptorPoolSize sizes[2] = {{VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER, 2}, {VK_DESCRIPTOR_TYPE_STORAGE_IMAGE, 1}};
    VkDescriptorPoolCreateInfo descriptors = {.sType = VK_STRUCTURE_TYPE_DESCRIPTOR_POOL_CREATE_INFO,
        .maxSets = 1, .poolSizeCount = 2, .pPoolSizes = sizes};
    VkShaderModuleCreateInfo shader = {.sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO,
        .codeSize = sizeof(pd_video_convert_spirv), .pCode = pd_video_convert_spirv};
    if (vkCreateCommandPool(vk->act_dev, &pool, vk->alloc, &decoder->pool) != VK_SUCCESS ||
        vkCreateSampler(vk->act_dev, &sampler, vk->alloc, &decoder->sampler) != VK_SUCCESS ||
        vkCreateDescriptorSetLayout(vk->act_dev, &set_layout, vk->alloc, &decoder->set_layout) != VK_SUCCESS ||
        vkCreateDescriptorPool(vk->act_dev, &descriptors, vk->alloc, &decoder->descriptors) != VK_SUCCESS ||
        vkCreateShaderModule(vk->act_dev, &shader, vk->alloc, &decoder->shader) != VK_SUCCESS) return AVERROR(ENOMEM);
    VkDescriptorSetAllocateInfo set = {.sType = VK_STRUCTURE_TYPE_DESCRIPTOR_SET_ALLOCATE_INFO,
        .descriptorPool = decoder->descriptors, .descriptorSetCount = 1, .pSetLayouts = &decoder->set_layout};
    VkPushConstantRange constants = {VK_SHADER_STAGE_COMPUTE_BIT, 0, sizeof(struct convert_parameters)};
    VkPipelineLayoutCreateInfo pipeline_layout = {.sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO,
        .setLayoutCount = 1, .pSetLayouts = &decoder->set_layout,
        .pushConstantRangeCount = 1, .pPushConstantRanges = &constants};
    if (vkAllocateDescriptorSets(vk->act_dev, &set, &decoder->set) != VK_SUCCESS ||
        vkCreatePipelineLayout(vk->act_dev, &pipeline_layout, vk->alloc, &decoder->pipeline_layout) != VK_SUCCESS)
        return AVERROR(ENOMEM);
    VkComputePipelineCreateInfo pipeline = {.sType = VK_STRUCTURE_TYPE_COMPUTE_PIPELINE_CREATE_INFO,
        .stage = {.sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO,
            .stage = VK_SHADER_STAGE_COMPUTE_BIT, .module = decoder->shader, .pName = "main"},
        .layout = decoder->pipeline_layout};
    if (vkCreateComputePipelines(vk->act_dev, VK_NULL_HANDLE, 1, &pipeline, vk->alloc, &decoder->pipeline) != VK_SUCCESS)
        return AVERROR(ENOSYS);
    return 0;
}
static void vulkan_close(struct pd_video_decoder *decoder) {
    av_buffer_unref(&decoder->export_frames);
    if (!decoder->device) return;
    AVVulkanDeviceContext *vk = ((AVHWDeviceContext *)decoder->device->data)->hwctx;
    vkDestroyPipeline(vk->act_dev, decoder->pipeline, vk->alloc);
    vkDestroyPipelineLayout(vk->act_dev, decoder->pipeline_layout, vk->alloc);
    vkDestroyShaderModule(vk->act_dev, decoder->shader, vk->alloc);
    vkDestroyDescriptorPool(vk->act_dev, decoder->descriptors, vk->alloc);
    vkDestroyDescriptorSetLayout(vk->act_dev, decoder->set_layout, vk->alloc);
    vkDestroySampler(vk->act_dev, decoder->sampler, vk->alloc);
    vkDestroyCommandPool(vk->act_dev, decoder->pool, vk->alloc);
    av_buffer_unref(&decoder->device);
}
/* The driver lists its preferred layout first; on NVIDIA that is a
 * block-linear one, which its EGL can bind as an ordinary texture (a linear
 * image is external-only there). The image must be writable by the compute
 * pass and exportable as a dma-buf. */
static int modifier_usable(VkPhysicalDevice device, VkImageUsageFlags usage, uint64_t modifier) {
    VkPhysicalDeviceImageDrmFormatModifierInfoEXT layout = {
        .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_IMAGE_DRM_FORMAT_MODIFIER_INFO_EXT,
        .drmFormatModifier = modifier, .sharingMode = VK_SHARING_MODE_EXCLUSIVE};
    VkPhysicalDeviceExternalImageFormatInfo external = {
        .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_EXTERNAL_IMAGE_FORMAT_INFO, .pNext = &layout,
        .handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT};
    VkPhysicalDeviceImageFormatInfo2 image = {.sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_IMAGE_FORMAT_INFO_2,
        .pNext = &external, .format = VK_FORMAT_R8G8B8A8_UNORM, .type = VK_IMAGE_TYPE_2D,
        .tiling = VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT, .usage = usage};
    VkExternalImageFormatProperties exported = {.sType = VK_STRUCTURE_TYPE_EXTERNAL_IMAGE_FORMAT_PROPERTIES};
    VkImageFormatProperties2 supported = {.sType = VK_STRUCTURE_TYPE_IMAGE_FORMAT_PROPERTIES_2, .pNext = &exported};
    return vkGetPhysicalDeviceImageFormatProperties2(device, &image, &supported) == VK_SUCCESS &&
        (exported.externalMemoryProperties.externalMemoryFeatures & VK_EXTERNAL_MEMORY_FEATURE_EXPORTABLE_BIT);
}
static int export_modifier(VkPhysicalDevice device, VkImageUsageFlags usage, uint32_t height, uint64_t *modifier) {
    VkDrmFormatModifierPropertiesEXT found[64];
    VkDrmFormatModifierPropertiesListEXT list = {.sType = VK_STRUCTURE_TYPE_DRM_FORMAT_MODIFIER_PROPERTIES_LIST_EXT};
    VkFormatProperties2 properties = {.sType = VK_STRUCTURE_TYPE_FORMAT_PROPERTIES_2, .pNext = &list};
    vkGetPhysicalDeviceFormatProperties2(device, VK_FORMAT_R8G8B8A8_UNORM, &properties);
    if (list.drmFormatModifierCount > 64) list.drmFormatModifierCount = 64;
    list.pDrmFormatModifierProperties = found;
    vkGetPhysicalDeviceFormatProperties2(device, VK_FORMAT_R8G8B8A8_UNORM, &properties);
    for (uint32_t i = 0; i < list.drmFormatModifierCount; i++) {
        if (found[i].drmFormatModifierPlaneCount != 1 ||
            !(found[i].drmFormatModifierTilingFeatures & VK_FORMAT_FEATURE_STORAGE_IMAGE_BIT)) continue;
        uint64_t chosen = found[i].drmFormatModifier;
        if (!modifier_usable(device, usage, chosen)) continue;
        /* NVIDIA block-linear (vendor 0x03): the low four bits are log2 of the
         * block height in 8-row units. The driver lays a short image out with
         * a smaller block than the one requested (halving while the image fits
         * in half a block) yet still reports the requested modifier, even from
         * vkGetImageDrmFormatModifierPropertiesEXT. Measured on 595.99: a
         * 128-row image requested as ...15 is laid out as ...14, and EGL
         * rejects the import under the former. Name the real layout instead. */
        if (chosen >> 56 == 0x03) {
            uint64_t log2_block = chosen & 0xf;
            while (log2_block > 0 && height <= (4u << log2_block)) log2_block--;
            uint64_t fitted = (chosen & ~(uint64_t)0xf) | log2_block;
            if (fitted != chosen && modifier_usable(device, usage, fitted)) chosen = fitted;
        }
        *modifier = chosen;
        return 0;
    }
    return AVERROR(ENOSYS);
}
static void free_modifier_list(AVHWFramesContext *frames) { free(frames->user_opaque); }
/* A pool of RGBA images the compositor can import. Rebuilt when the visible
 * size changes (a mid-stream resolution switch). */
static int export_pool(struct pd_video_decoder *decoder, const AVFrame *source) {
    if (decoder->export_frames) {
        AVHWFramesContext *current = (AVHWFramesContext *)decoder->export_frames->data;
        if (current->width == source->width && current->height == source->height) return 0;
        av_buffer_unref(&decoder->export_frames);
    }
    const VkImageUsageFlags usage = VK_IMAGE_USAGE_STORAGE_BIT | VK_IMAGE_USAGE_TRANSFER_SRC_BIT | VK_IMAGE_USAGE_SAMPLED_BIT;
    AVVulkanDeviceContext *device = ((AVHWDeviceContext *)decoder->device->data)->hwctx;
    int rc = export_modifier(device->phys_dev, usage, (uint32_t)source->height, &decoder->export_modifier);
    if (rc < 0) return rc;
    struct { VkImageDrmFormatModifierListCreateInfoEXT list; uint64_t modifier; } *modifiers = calloc(1, sizeof(*modifiers));
    if (!modifiers) return AVERROR(ENOMEM);
    modifiers->modifier = decoder->export_modifier;
    modifiers->list = (VkImageDrmFormatModifierListCreateInfoEXT) {
        .sType = VK_STRUCTURE_TYPE_IMAGE_DRM_FORMAT_MODIFIER_LIST_CREATE_INFO_EXT,
        .drmFormatModifierCount = 1, .pDrmFormatModifiers = &modifiers->modifier};
    AVBufferRef *reference = av_hwframe_ctx_alloc(decoder->device);
    if (!reference) { free(modifiers); return AVERROR(ENOMEM); }
    AVHWFramesContext *frames = (AVHWFramesContext *)reference->data;
    AVVulkanFramesContext *vk = frames->hwctx;
    frames->format = AV_PIX_FMT_VULKAN;
    frames->sw_format = AV_PIX_FMT_RGBA;
    frames->width = source->width;
    frames->height = source->height;
    frames->user_opaque = modifiers; /* create_pnext is read on every allocation. */
    frames->free = free_modifier_list;
    vk->tiling = VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT;
    vk->usage = usage;
    vk->create_pnext = &modifiers->list;
    rc = av_hwframe_ctx_init(reference);
    if (rc < 0) { av_buffer_unref(&reference); return rc; }
    decoder->export_frames = reference;
    return 0;
}
/* Y'CbCr -> R'G'B' for the stream's matrix and range. An untagged stream is
 * treated the way players conventionally do: BT.709 from 720 lines up, BT.601
 * below, limited range. */
static struct convert_parameters conversion(const AVFrame *frame, float chroma_zero) {
    double kr, kb;
    switch (frame->colorspace) {
    case AVCOL_SPC_BT709: kr = 0.2126; kb = 0.0722; break;
    case AVCOL_SPC_BT470BG: case AVCOL_SPC_SMPTE170M: kr = 0.299; kb = 0.114; break;
    case AVCOL_SPC_BT2020_NCL: case AVCOL_SPC_BT2020_CL: kr = 0.2627; kb = 0.0593; break;
    default:
        if (frame->height >= 720) { kr = 0.2126; kb = 0.0722; } else { kr = 0.299; kb = 0.114; }
    }
    double kg = 1.0 - kr - kb;
    int full = frame->color_range == AVCOL_RANGE_JPEG;
    return (struct convert_parameters) {
        .range = {full ? 0.0f : 16.0f / 255.0f, full ? 1.0f : 255.0f / 219.0f, full ? 1.0f : 255.0f / 224.0f, chroma_zero},
        .matrix = {(float)(2.0 * (1.0 - kr)), (float)(2.0 * (1.0 - kb) * kb / kg),
                   (float)(2.0 * (1.0 - kr) * kr / kg), (float)(2.0 * (1.0 - kb))}};
}
/* Convert the decoded two-plane image into `target` on the GPU and wait for
 * it, so the exported buffer is complete before another process reads it.
 * Follows FFmpeg's AVVkFrame contract: wait for each image's timeline
 * semaphore at its current value, signal value + 1, record the new layout. */
static int convert(struct pd_video_decoder *decoder, AVFrame *source, AVFrame *target) {
    AVHWDeviceContext *device = (AVHWDeviceContext *)decoder->device->data;
    AVVulkanDeviceContext *vk = device->hwctx;
    AVHWFramesContext *source_frames = (AVHWFramesContext *)source->hw_frames_ctx->data;
    AVHWFramesContext *target_frames = (AVHWFramesContext *)target->hw_frames_ctx->data;
    AVVulkanFramesContext *source_vk = source_frames->hwctx, *target_vk = target_frames->hwctx;
    AVVkFrame *from = (AVVkFrame *)source->data[0], *to = (AVVkFrame *)target->data[0];
    VkFormat plane_formats[2];
    /* "No colour" is code 128 of 255 (or 512 of 1023), not exactly one half. */
    float chroma_zero = 128.0f / 255.0f;
    switch (source_frames->sw_format) {
    case AV_PIX_FMT_NV12: plane_formats[0] = VK_FORMAT_R8_UNORM; plane_formats[1] = VK_FORMAT_R8G8_UNORM; break;
    /* 10 bits in the high end of 16: the UNORM value is the sample to 0.1 %. */
    case AV_PIX_FMT_P010: plane_formats[0] = VK_FORMAT_R16_UNORM; plane_formats[1] = VK_FORMAT_R16G16_UNORM;
        chroma_zero = 32768.0f / 65535.0f; break;
    default: return AVERROR(ENOSYS);
    }
    /* One two-plane image whose planes can be viewed and sampled on their own. */
    if (!from || !to || !from->img[0] || from->img[1] || !to->img[0] || to->img[1] ||
        !(source_vk->usage & VK_IMAGE_USAGE_SAMPLED_BIT) ||
        !(source_vk->img_flags & VK_IMAGE_CREATE_MUTABLE_FORMAT_BIT)) return AVERROR(ENOSYS);
    VkImageView views[3] = {VK_NULL_HANDLE, VK_NULL_HANDLE, VK_NULL_HANDLE};
    VkCommandBuffer commands = VK_NULL_HANDLE;
    VkFence fence = VK_NULL_HANDLE;
    int rc = AVERROR(EIO), locked = 0;
    /* A plane view may only claim usages its single-plane format supports. */
    VkImageViewUsageCreateInfo sampled = {.sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_USAGE_CREATE_INFO,
        .usage = VK_IMAGE_USAGE_SAMPLED_BIT};
    VkImageViewCreateInfo view_info[3] = {
        {.sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO, .pNext = &sampled, .image = from->img[0],
         .viewType = VK_IMAGE_VIEW_TYPE_2D, .format = plane_formats[0],
         .subresourceRange = {VK_IMAGE_ASPECT_PLANE_0_BIT, 0, 1, 0, 1}},
        {.sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO, .pNext = &sampled, .image = from->img[0],
         .viewType = VK_IMAGE_VIEW_TYPE_2D, .format = plane_formats[1],
         .subresourceRange = {VK_IMAGE_ASPECT_PLANE_1_BIT, 0, 1, 0, 1}},
        {.sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO, .image = to->img[0],
         .viewType = VK_IMAGE_VIEW_TYPE_2D, .format = VK_FORMAT_R8G8B8A8_UNORM,
         .subresourceRange = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1}}};
    for (int i = 0; i < 3; i++)
        if (vkCreateImageView(vk->act_dev, &view_info[i], vk->alloc, &views[i]) != VK_SUCCESS) { views[i] = VK_NULL_HANDLE; goto done; }
    VkCommandBufferAllocateInfo allocate = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
        .commandPool = decoder->pool, .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount = 1};
    VkFenceCreateInfo fence_info = {.sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO};
    if (vkAllocateCommandBuffers(vk->act_dev, &allocate, &commands) != VK_SUCCESS) { commands = VK_NULL_HANDLE; goto done; }
    if (vkCreateFence(vk->act_dev, &fence_info, vk->alloc, &fence) != VK_SUCCESS) { fence = VK_NULL_HANDLE; goto done; }
    /* The set is reused: the previous frame's pass has completed (see the
     * fence wait below), so nothing still reads these descriptors. */
    VkDescriptorImageInfo images[3] = {
        {decoder->sampler, views[0], VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL},
        {decoder->sampler, views[1], VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL},
        {VK_NULL_HANDLE, views[2], VK_IMAGE_LAYOUT_GENERAL}};
    VkWriteDescriptorSet writes[3];
    for (uint32_t i = 0; i < 3; i++)
        writes[i] = (VkWriteDescriptorSet) {.sType = VK_STRUCTURE_TYPE_WRITE_DESCRIPTOR_SET,
            .dstSet = decoder->set, .dstBinding = i, .descriptorCount = 1,
            .descriptorType = i < 2 ? VK_DESCRIPTOR_TYPE_COMBINED_IMAGE_SAMPLER : VK_DESCRIPTOR_TYPE_STORAGE_IMAGE,
            .pImageInfo = &images[i]};
    vkUpdateDescriptorSets(vk->act_dev, 3, writes, 0, NULL);
    source_vk->lock_frame(source_frames, from);
    target_vk->lock_frame(target_frames, to);
    locked = 1;
    VkCommandBufferBeginInfo begin = {.sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
        .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT};
    if (vkBeginCommandBuffer(commands, &begin) != VK_SUCCESS) goto done;
    VkImageSubresourceRange whole = {VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1};
    VkImageMemoryBarrier barriers[2] = {
        {.sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER, .dstAccessMask = VK_ACCESS_SHADER_READ_BIT,
         .oldLayout = from->layout[0], .newLayout = VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL,
         .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED, .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
         .image = from->img[0], .subresourceRange = whole},
        /* The previous contents of a recycled target are never needed. */
        {.sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER, .dstAccessMask = VK_ACCESS_SHADER_WRITE_BIT,
         .oldLayout = VK_IMAGE_LAYOUT_UNDEFINED, .newLayout = VK_IMAGE_LAYOUT_GENERAL,
         .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED, .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
         .image = to->img[0], .subresourceRange = whole}};
    vkCmdPipelineBarrier(commands, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_COMPUTE_SHADER_BIT,
        0, 0, NULL, 0, NULL, 2, barriers);
    struct convert_parameters parameters = conversion(source, chroma_zero);
    vkCmdBindPipeline(commands, VK_PIPELINE_BIND_POINT_COMPUTE, decoder->pipeline);
    vkCmdBindDescriptorSets(commands, VK_PIPELINE_BIND_POINT_COMPUTE, decoder->pipeline_layout, 0, 1, &decoder->set, 0, NULL);
    vkCmdPushConstants(commands, decoder->pipeline_layout, VK_SHADER_STAGE_COMPUTE_BIT, 0, sizeof(parameters), &parameters);
    vkCmdDispatch(commands, ((uint32_t)source->width + 7) / 8, ((uint32_t)source->height + 7) / 8, 1);
    if (vkEndCommandBuffer(commands) != VK_SUCCESS) goto done;
    VkSemaphore semaphores[2] = {from->sem[0], to->sem[0]};
    uint64_t wait_values[2] = {from->sem_value[0], to->sem_value[0]};
    uint64_t signal_values[2] = {wait_values[0] + 1, wait_values[1] + 1};
    VkPipelineStageFlags stages[2] = {VK_PIPELINE_STAGE_ALL_COMMANDS_BIT, VK_PIPELINE_STAGE_ALL_COMMANDS_BIT};
    VkTimelineSemaphoreSubmitInfo timeline = {.sType = VK_STRUCTURE_TYPE_TIMELINE_SEMAPHORE_SUBMIT_INFO,
        .waitSemaphoreValueCount = 2, .pWaitSemaphoreValues = wait_values,
        .signalSemaphoreValueCount = 2, .pSignalSemaphoreValues = signal_values};
    VkSubmitInfo submit = {.sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .pNext = &timeline,
        .waitSemaphoreCount = 2, .pWaitSemaphores = semaphores, .pWaitDstStageMask = stages,
        .commandBufferCount = 1, .pCommandBuffers = &commands,
        .signalSemaphoreCount = 2, .pSignalSemaphores = semaphores};
    vk->lock_queue(device, decoder->queue_family, 0);
    VkResult submitted = vkQueueSubmit(decoder->queue, 1, &submit, fence);
    vk->unlock_queue(device, decoder->queue_family, 0);
    if (submitted != VK_SUCCESS) goto done;
    /* Submitted: the semaphores will advance whether or not the wait succeeds. */
    from->sem_value[0]++; to->sem_value[0]++;
    from->layout[0] = VK_IMAGE_LAYOUT_SHADER_READ_ONLY_OPTIMAL; from->access[0] = VK_ACCESS_SHADER_READ_BIT;
    to->layout[0] = VK_IMAGE_LAYOUT_GENERAL; to->access[0] = VK_ACCESS_SHADER_WRITE_BIT;
    if (vkWaitForFences(vk->act_dev, 1, &fence, VK_TRUE, 5000000000ull) != VK_SUCCESS) {
        /* The pass may still be running; nothing it uses may be freed yet. */
        vkDeviceWaitIdle(vk->act_dev);
        rc = AVERROR(ETIMEDOUT); goto done;
    }
    rc = 0;
done:
    if (locked) {
        target_vk->unlock_frame(target_frames, to);
        source_vk->unlock_frame(source_frames, from);
    }
    if (fence) vkDestroyFence(vk->act_dev, fence, vk->alloc);
    if (commands) vkFreeCommandBuffers(vk->act_dev, decoder->pool, 1, &commands);
    for (int i = 0; i < 3; i++) vkDestroyImageView(vk->act_dev, views[i], vk->alloc);
    return rc;
}
static int export_vulkan(struct pd_video_decoder *decoder, struct pd_video_frame *frame,
    struct pd_video_frame_info *info, const char **stage) {
    *stage = "allocate exportable Vulkan frame";
    AVFrame *decoded = frame->frame;
    if (decoded->format != AV_PIX_FMT_VULKAN || !decoded->hw_frames_ctx) return AVERROR(ENOSYS);
    int rc = export_pool(decoder, decoded);
    if (rc < 0) return rc;
    AVFrame *converted = av_frame_alloc();
    if (!converted) return AVERROR(ENOMEM);
    rc = av_hwframe_get_buffer(decoder->export_frames, converted, 0);
    if (rc >= 0) { *stage = "convert decoded Vulkan frame on the GPU"; rc = convert(decoder, decoded, converted); }
    if (rc < 0) { av_frame_free(&converted); return rc; }
    converted->width = decoded->width; converted->height = decoded->height; converted->pts = decoded->pts;
    /* The decode image is no longer needed: the frame keeps only the converted
     * one, so a held frame never pins the decoder's reference pool. */
    av_frame_free(&frame->frame);
    frame->frame = converted;
    /* Exported by hand; see the note on av_hwframe_map() at the top. */
    *stage = "export Vulkan frame as dma-buf";
    AVVulkanDeviceContext *vk = ((AVHWDeviceContext *)decoder->device->data)->hwctx;
    AVVkFrame *image = (AVVkFrame *)converted->data[0];
    PFN_vkGetDeviceProcAddr device_proc = (PFN_vkGetDeviceProcAddr)vk->get_proc_addr(vk->inst, "vkGetDeviceProcAddr");
    PFN_vkGetMemoryFdKHR memory_fd = device_proc ? (PFN_vkGetMemoryFdKHR)device_proc(vk->act_dev, "vkGetMemoryFdKHR") : NULL;
    if (!memory_fd || !image->mem[0] || image->mem[1] || image->size[0] > UINT32_MAX) return AVERROR(ENOSYS);
    VkMemoryGetFdInfoKHR request = {.sType = VK_STRUCTURE_TYPE_MEMORY_GET_FD_INFO_KHR,
        .memory = image->mem[0], .handleType = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT};
    int fd = -1;
    if (memory_fd(vk->act_dev, &request, &fd) != VK_SUCCESS || fd < 0) return AVERROR(EIO);
    frame->fds[frame->fd_count++] = fd; /* Closed with the frame, also on the failure below. */
    VkImageSubresource plane = {.aspectMask = VK_IMAGE_ASPECT_MEMORY_PLANE_0_BIT_EXT};
    VkSubresourceLayout placed;
    vkGetImageSubresourceLayout(vk->act_dev, image->img[0], &plane, &placed);
    if (placed.offset > UINT32_MAX || placed.rowPitch == 0 || placed.rowPitch > UINT32_MAX) return AVERROR(EINVAL);
    /* R8G8B8A8 in memory order is DRM "AB24"; the pass writes alpha = 1, and
     * "XB24" tells the compositor the buffer is opaque. The pool offers the
     * driver exactly one modifier, so that is the image's modifier. */
    info->fourcc = 0x34324258;
    info->object_count = 1;
    info->plane_count = 1;
    info->objects[0] = (struct pd_video_object) {fd, (uint32_t)image->size[0], decoder->export_modifier};
    info->planes[0] = (struct pd_video_plane) {0, (uint32_t)placed.offset, (uint32_t)placed.rowPitch};
    return 0;
}
#endif

struct pd_video_decoder *pd_video_open(const char *name, const char *device,
    const uint8_t *extra, uint32_t extra_size, char *error, uint32_t error_size) {
    struct pd_video_decoder *decoder = NULL;
    const AVCodec *codec = NULL;
    int rc = AVERROR(EINVAL), supported = 0;
    const char *operation = "invalid codec/configuration";
    if (!name || !device || extra_size > 1024 * 1024 || (extra_size && !extra)) goto failed;
    const char *allowed[] = {"h264", "hevc", "vp8", "vp9", "av1"};
    for (unsigned i = 0; i < sizeof(allowed) / sizeof(*allowed); i++)
        if (!strcmp(name, allowed[i])) { codec = avcodec_find_decoder_by_name(name); break; }
    if (!codec) goto failed;
#ifdef PD_VIDEO_VAAPI
    supported |= codec_supports(codec, AV_HWDEVICE_TYPE_VAAPI);
#endif
#ifdef PD_VIDEO_VULKAN
    supported |= codec_supports(codec, AV_HWDEVICE_TYPE_VULKAN);
#endif
    if (!supported) { operation = "codec has no hardware decoder"; rc = AVERROR(ENOSYS); goto failed; }
    operation = "allocate decoder"; rc = AVERROR(ENOMEM);
    decoder = calloc(1, sizeof(*decoder));
    if (!decoder) goto failed;
    decoder->slots = calloc(1, sizeof(*decoder->slots));
    if (!decoder->slots) goto failed;
    atomic_init(&decoder->slots->refs, 1);
    atomic_init(&decoder->slots->held, 0);
    decoder->codec = avcodec_alloc_context3(codec);
    if (!decoder->codec) goto failed;
    /* VA-API first: it exports the decode surface without a copy. A host
     * whose driver has no VA-API (NVIDIA) falls through to Vulkan Video. */
    operation = "create hardware decode device"; rc = AVERROR(ENOSYS);
#ifdef PD_VIDEO_VAAPI
    if (codec_supports(codec, AV_HWDEVICE_TYPE_VAAPI)) {
        rc = av_hwdevice_ctx_create(&decoder->codec->hw_device_ctx, AV_HWDEVICE_TYPE_VAAPI, device, NULL, 0);
        if (rc >= 0) decoder->backend = BACKEND_VAAPI;
    }
#endif
#ifdef PD_VIDEO_VULKAN
    if (!decoder->backend && codec_supports(codec, AV_HWDEVICE_TYPE_VULKAN)) {
        rc = vulkan_device(decoder, device);
        if (rc >= 0) decoder->backend = BACKEND_VULKAN;
    }
#endif
    if (!decoder->backend) goto failed;
    decoder->codec->opaque = decoder;
    decoder->codec->get_format = hardware_only;
    decoder->codec->pkt_timebase = (AVRational){1, 1000000};
    decoder->codec->thread_count = 1;
    decoder->codec->max_pixels = 8192LL * 8192;
    decoder->codec->extra_hw_frames = PD_VIDEO_MAX_HELD;
    if (extra_size) {
        decoder->codec->extradata = av_mallocz(extra_size + AV_INPUT_BUFFER_PADDING_SIZE);
        if (!decoder->codec->extradata) { rc = AVERROR(ENOMEM); goto failed; }
        memcpy(decoder->codec->extradata, extra, extra_size);
        decoder->codec->extradata_size = (int)extra_size;
    }
    operation = "open hardware decoder";
    rc = avcodec_open2(decoder->codec, codec, NULL);
    if (rc < 0) goto failed;
    return decoder;
failed:
    fail(error, error_size, operation, rc);
    pd_video_close(decoder);
    return NULL;
}
void pd_video_close(struct pd_video_decoder *decoder) {
    if (!decoder) return;
    avcodec_free_context(&decoder->codec);
#ifdef PD_VIDEO_VULKAN
    vulkan_close(decoder);
#endif
    if (decoder->slots) unref_slots(decoder->slots);
    free(decoder);
}
const char *pd_video_backend(const struct pd_video_decoder *decoder) {
    if (!decoder) return "";
    return decoder->backend == BACKEND_VAAPI ? "vaapi" : "vulkan";
}
int pd_video_send(struct pd_video_decoder *decoder, const uint8_t *bytes,
    uint32_t size, int64_t pts, char *error, uint32_t error_size) {
    if (!decoder || size > 16 * 1024 * 1024 || (size && !bytes))
        return fail(error, error_size, "invalid packet", AVERROR(EINVAL));
    AVPacket *packet = NULL;
    int rc;
    if (bytes && size) {
        packet = av_packet_alloc();
        if (!packet) return fail(error, error_size, "allocate packet", AVERROR(ENOMEM));
        rc = av_new_packet(packet, (int)size);
        if (rc < 0) { av_packet_free(&packet); return fail(error, error_size, "allocate packet data", rc); }
        memcpy(packet->data, bytes, size);
        packet->pts = pts;
        packet->dts = AV_NOPTS_VALUE;
    }
    rc = avcodec_send_packet(decoder->codec, packet);
    av_packet_free(&packet);
    if (rc == AVERROR(EAGAIN)) return 1;
    return rc < 0 ? fail(error, error_size, "send hardware packet", rc) : 0;
}
void pd_video_frame_release(struct pd_video_frame *frame) {
    if (!frame) return;
    for (uint32_t i = 0; i < frame->fd_count; i++) close(frame->fds[i]);
    av_frame_free(&frame->frame);
    if (frame->slots) {
        atomic_fetch_sub(&frame->slots->held, 1);
        unref_slots(frame->slots);
    }
    free(frame);
}
int pd_video_receive(struct pd_video_decoder *decoder, struct pd_video_frame **out,
    struct pd_video_frame_info *info, char *error, uint32_t error_size) {
    if (!decoder || !out || !info) return fail(error, error_size, "invalid output", AVERROR(EINVAL));
    *out = NULL;
    if (atomic_load(&decoder->slots->held) >= PD_VIDEO_MAX_HELD) return 3;
    struct pd_video_frame *frame = calloc(1, sizeof(*frame));
    if (!frame) return fail(error, error_size, "allocate frame", AVERROR(ENOMEM));
    frame->backend = decoder->backend;
    frame->frame = av_frame_alloc();
    if (!frame->frame) { free(frame); return fail(error, error_size, "allocate AVFrame", AVERROR(ENOMEM)); }
    int rc = avcodec_receive_frame(decoder->codec, frame->frame);
    if (rc < 0) {
        pd_video_frame_release(frame);
        if (rc == AVERROR(EAGAIN)) return 1;
        if (rc == AVERROR_EOF) return 2;
        return fail(error, error_size, "receive hardware frame", rc);
    }
    const char *stage = "export hardware frame";
    memset(info, 0, sizeof(*info));
    rc = AVERROR(ENOSYS);
#ifdef PD_VIDEO_VAAPI
    if (decoder->backend == BACKEND_VAAPI) rc = export_vaapi(frame, info, &stage);
#endif
#ifdef PD_VIDEO_VULKAN
    if (decoder->backend == BACKEND_VULKAN) rc = export_vulkan(decoder, frame, info, &stage);
#endif
    if (rc < 0) { pd_video_frame_release(frame); return fail(error, error_size, stage, rc); }
    info->width = (uint32_t)frame->frame->width;
    info->height = (uint32_t)frame->frame->height;
    info->pts_us = frame->frame->pts;
    frame->slots = decoder->slots;
    atomic_fetch_add(&frame->slots->refs, 1);
    atomic_fetch_add(&frame->slots->held, 1);
    *out = frame;
    return 0;
}
void pd_video_flush(struct pd_video_decoder *decoder) {
    if (decoder) avcodec_flush_buffers(decoder->codec);
}
/* Diagnostics below download the frame to the CPU. Playback never calls them. */
static int download(struct pd_video_frame *frame, enum AVPixelFormat format, AVFrame **out) {
    AVFrame *cpu = av_frame_alloc();
    if (!cpu) return AVERROR(ENOMEM);
    cpu->format = format;
    int rc = av_hwframe_transfer_data(cpu, frame->frame, 0);
    if (rc < 0) av_frame_free(&cpu); else *out = cpu;
    return rc;
}
int pd_video_test_luma(struct pd_video_frame *frame, uint32_t *sample,
    char *error, uint32_t error_size) {
    if (!frame || !sample) return fail(error, error_size, "invalid diagnostic frame", AVERROR(EINVAL));
    /* A converted frame has no luma plane left to read; see pd_video_test_rgba. */
    if (frame->backend != BACKEND_VAAPI)
        return fail(error, error_size, "luma readback needs a YUV frame; this backend exports RGBA", AVERROR(ENOSYS));
    AVFrame *cpu = NULL;
    int rc = download(frame, AV_PIX_FMT_NONE, &cpu);
    if (rc < 0) return fail(error, error_size, "diagnostic GPU readback", rc);
    *sample = cpu->data[0][0];
    av_frame_free(&cpu);
    return 0;
}
int pd_video_test_rgba(struct pd_video_frame *frame, uint8_t *pixels, uint32_t capacity,
    char *error, uint32_t error_size) {
    if (!frame || !pixels) return fail(error, error_size, "invalid diagnostic frame", AVERROR(EINVAL));
    if (frame->backend != BACKEND_VULKAN)
        return fail(error, error_size, "RGBA readback needs an RGBA frame; this backend exports YUV", AVERROR(ENOSYS));
    AVFrame *cpu = NULL;
    int rc = download(frame, AV_PIX_FMT_RGBA, &cpu);
    if (rc < 0) return fail(error, error_size, "diagnostic GPU readback", rc);
    size_t row = (size_t)cpu->width * 4;
    if (cpu->format != AV_PIX_FMT_RGBA || (size_t)capacity < row * (size_t)cpu->height) rc = AVERROR(EINVAL);
    else for (int y = 0; y < cpu->height; y++) memcpy(pixels + row * (size_t)y, cpu->data[0] + (ptrdiff_t)cpu->linesize[0] * y, row);
    av_frame_free(&cpu);
    return rc < 0 ? fail(error, error_size, "diagnostic RGBA readback", rc) : 0;
}
