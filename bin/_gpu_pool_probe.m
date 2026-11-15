// bin/_gpu_pool_probe.m — the memory the graphics driver charges a
// process for as long as it has the GPU execute render passes, on this
// machine, in megabytes on stdout.
//
// The driver gives every such process a pool of a fixed size that
// depends on the GPU (about 110 MB on an M4 Pro, 230 MB on an M4 Max)
// and takes it back a second or so after the last pass.  The core pays
// it while it draws; a check of the core's footprint while drawing has
// to know how much of it is the machine's.
//
//   clang -fobjc-arc -framework Metal -framework Foundation _gpu_pool_probe.m -o probe
//
// No window, nothing signalled: twenty small render passes, the
// footprint before the first and after the last.
#import <Foundation/Foundation.h>
#import <Metal/Metal.h>
#include <libproc.h>
#include <unistd.h>

static double mb(void) {
    struct rusage_info_v4 r;
    proc_pid_rusage(getpid(), RUSAGE_INFO_V4, (rusage_info_t *)&r);
    return r.ri_phys_footprint / 1048576.0;
}

int main(void) {
    @autoreleasepool {
        id<MTLDevice> dev = MTLCreateSystemDefaultDevice();
        if (!dev) return 1;
        id<MTLCommandQueue> q = [dev newCommandQueue];
        MTLTextureDescriptor *d = [MTLTextureDescriptor texture2DDescriptorWithPixelFormat:MTLPixelFormatBGRA8Unorm width:256 height:256 mipmapped:NO];
        d.usage = MTLTextureUsageRenderTarget;
        d.storageMode = MTLStorageModePrivate;
        id<MTLTexture> tex = [dev newTextureWithDescriptor:d];
        double before = mb();
        for (int i = 0; i < 20; i++) {
            @autoreleasepool {
                MTLRenderPassDescriptor *rp = [MTLRenderPassDescriptor renderPassDescriptor];
                rp.colorAttachments[0].texture = tex;
                rp.colorAttachments[0].loadAction = MTLLoadActionClear;
                rp.colorAttachments[0].storeAction = MTLStoreActionStore;
                id<MTLCommandBuffer> cmd = [q commandBuffer];
                [[cmd renderCommandEncoderWithDescriptor:rp] endEncoding];
                [cmd commit];
                [cmd waitUntilCompleted];
            }
            usleep(20000);
        }
        printf("%.0f\n", mb() - before);
    }
    return 0;
}
