<template>
  <div ref="videoContainer" class="relative w-full h-full overflow-hidden bg-black">
    <video
      v-if="videoUrl || isPreparing"
      :id="playerId"
      class="video-js vjs-big-play-centered"
      playsinline
    ></video>

    <div v-if="isPreparing && currentStage === 'remux'" class="absolute inset-0 flex items-center justify-center bg-black/60 z-20 pointer-events-none">
      <div class="flex flex-col items-center gap-3">
        <span class="loading loading-spinner loading-lg text-primary"></span>
        <span class="text-white/70 text-sm font-medium">{{ $t('video.preparing') || '正在处理视频...' }}</span>
      </div>
    </div>

    <div v-if="isPreparing && currentStage === 'transcode'" class="absolute inset-0 flex items-center justify-center bg-black/60 z-20 pointer-events-none">
      <div class="flex flex-col items-center gap-3">
        <span class="loading loading-spinner loading-lg text-primary"></span>
        <span class="text-white/70 text-sm font-medium">{{ $t('video.transcoding') || '正在转码视频...' }}</span>
      </div>
    </div>

    <div v-if="isUnsupported" class="absolute inset-0 flex flex-col items-center justify-center text-white/70 z-30 bg-black/80 px-6 text-center">
      <IconVideoSlash class="w-12 h-12 mb-3" />
      <div class="max-w-md whitespace-pre-line">{{ unsupportedMessage }}</div>
      <div v-if="canOpenExternalApp" class="mt-4">
        <button class="btn btn-primary btn-sm" @click="openInExternalApp">{{ externalOpenLabel }}</button>
      </div>
    </div>

    <div v-if="hasError" class="absolute inset-0 flex flex-col items-center justify-center text-white/70 z-30 bg-black/80 px-6 text-center">
      <IconVideoSlash class="w-12 h-12 mb-3" />
      <div class="max-w-md whitespace-pre-line">{{ errorMessage }}</div>
      <div class="mt-4 flex items-center gap-2 pointer-events-auto">
        <button class="btn btn-outline btn-sm text-white" @click="loadVideo(props.filePath!)">Retry</button>
        <button v-if="canOpenExternalApp" class="btn btn-primary btn-sm" @click="openInExternalApp">{{ externalOpenLabel }}</button>
      </div>
    </div>
  </div>
</template>

<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, ref, watch } from 'vue';
import { convertFileSrc } from '@tauri-apps/api/core';
import { useI18n } from 'vue-i18n';
import videojs from 'video.js';
import 'video.js/dist/video-js.css';

import { openFileWithApp } from '@/common/api';
import { config } from '@/common/config';
import { IconVideoSlash } from '@/common/icons';
import { cancelVideoPrepare, prepareVideo, type VideoPrepareResult } from '@/common/video';

const props = defineProps({
  filePath: { type: String, required: false },
  rotate: { type: Number, default: 0 },
  isSlideShow: { type: Boolean, default: false },
});

const emit = defineEmits(['message-from-video-viewer', 'slideshow-next', 'scale', 'viewport-change']);
const { t: $t } = useI18n();

const player = ref<ReturnType<typeof videojs> | null>(null);
const playerId = 'vjs-main-player';

const currentStage = ref<'direct' | 'remux' | 'transcode' | null>(null);
const isPreparing = ref(false);
const hasError = ref(false);
const isUnsupported = ref(false);
const errorMessage = ref('');
const unsupportedMessage = computed(() => $t('video.errors.unsupported_external') || '此视频格式不受支持，建议使用 IINA 或 VLC 播放');
const videoUrl = ref<string | null>(null);
let loadingId = 0;

const externalVideoAppPath = computed(() => String(config.settings?.externalVideoAppPath || '').trim());
const externalVideoAppName = computed(() => String(config.settings?.externalVideoAppName || '').trim());
const canOpenExternalApp = computed(() => !!(props.filePath && externalVideoAppPath.value));
const externalOpenLabel = computed(() => {
  if (externalVideoAppName.value) {
    return $t('video.errors.open_in_external_app_named', { app: externalVideoAppName.value }) || `Open in ${externalVideoAppName.value}`;
  }
  return $t('video.errors.open_in_external_app') || 'Open in external player';
});

function disposePlayer() {
  if (player.value) {
    try { player.value.dispose(); } catch {}
    player.value = null;
  }
}

function pausePlayer() {
  if (player.value) {
    try { player.value.pause(); } catch {}
  }
}

async function tryPlay(result: VideoPrepareResult, currentLoadId: number): Promise<boolean> {
  if (currentLoadId !== loadingId) return false;
  await nextTick();

  return new Promise((resolve) => {
    let settled = false;
    let timeoutId: ReturnType<typeof setTimeout> | null = null;
    const finalize = (ok: boolean) => {
      if (settled) return;
      settled = true;
      if (timeoutId) clearTimeout(timeoutId);
      resolve(ok);
    };

    const p = player.value ?? videojs(playerId, {
      autoplay: config.settings?.autoPlayVideo || props.isSlideShow,
      controls: false,
      fluid: true,
      preload: 'auto',
      muted: !!config.video?.muted,
      volume: config.video?.volume ?? 1,
    });
    player.value = p;

    p.off('loadedmetadata');
    p.off('error');

    timeoutId = setTimeout(() => {
      if (currentLoadId !== loadingId) return;
      finalize(false);
    }, 5000);

    p.src({
      src: convertFileSrc(result.url),
      type: result.mime_type,
    });
    p.load();
    if (config.settings?.autoPlayVideo || props.isSlideShow) {
      p.play().catch(() => {});
    }

    p.one('loadedmetadata', () => {
      if (currentLoadId !== loadingId) return;
      finalize(true);
    });
    p.one('error', () => {
      if (currentLoadId !== loadingId) return;
      finalize(false);
    });

    if (p.readyState() >= 1) {
      finalize(true);
    }
  });
}

async function loadVideo(filePath: string) {
  if (!filePath) return;
  const currentLoadId = ++loadingId;
  pausePlayer();
  videoUrl.value = filePath;
  hasError.value = false;
  isUnsupported.value = false;
  errorMessage.value = '';
  isPreparing.value = false;
  currentStage.value = 'direct';

  try {
    const res = await prepareVideo(filePath, playerId, null);
    if (currentLoadId !== loadingId) return;
    videoUrl.value = res.url;
    if (await tryPlay(res, currentLoadId)) {
      return;
    }
  } catch (e) {
    if (String(e).includes('ProbeError')) {
      hasError.value = true;
      errorMessage.value = $t('video.errors.unknown') || '未知错误';
      return;
    }
  }

  if (currentLoadId !== loadingId) return;
  currentStage.value = 'remux';
  isPreparing.value = true;
  try {
    const res = await prepareVideo(filePath, playerId, 'remux');
    if (currentLoadId !== loadingId) return;
    videoUrl.value = res.url;
    if (await tryPlay(res, currentLoadId)) {
      isPreparing.value = false;
      return;
    }
  } catch {}

  if (currentLoadId !== loadingId) return;
  currentStage.value = 'transcode';
  isPreparing.value = true;
  try {
    const res = await prepareVideo(filePath, playerId, 'transcode');
    if (currentLoadId !== loadingId) return;
    videoUrl.value = res.url;
    if (await tryPlay(res, currentLoadId)) {
      isPreparing.value = false;
      return;
    }
  } catch (e: any) {
    if (String(e).includes('DurationExceeded')) {
      isPreparing.value = false;
      isUnsupported.value = true;
      return;
    }
    if (String(e).includes('ProbeError')) {
      isPreparing.value = false;
      hasError.value = true;
      errorMessage.value = $t('video.errors.unknown') || '未知错误';
      return;
    }
  }

  if (currentLoadId !== loadingId) return;
  isPreparing.value = false;
  hasError.value = true;
  errorMessage.value = $t('video.errors.playback_failed') || '无法播放此视频';
}

async function openInExternalApp() {
  if (!props.filePath || !externalVideoAppPath.value) return;
  await openFileWithApp(props.filePath, externalVideoAppPath.value);
}

watch(() => props.filePath, (v) => v && loadVideo(v), { immediate: true });
onBeforeUnmount(() => {
  disposePlayer();
  if (props.filePath) {
    cancelVideoPrepare(playerId);
  }
});
</script>

<style scoped>
:deep(.video-js) {
  background-color: transparent;
}
:deep(.vjs-tech) {
  object-fit: contain;
}
</style>
