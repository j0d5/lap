import { invoke } from '@tauri-apps/api/core';

export interface VideoPrepareResult {
  url: string;
  mime_type: string;
  action: string;
  duration_secs: number;
}

export async function prepareVideo(
  filePath: string, 
  playerId: string, 
  force: string | null = null
): Promise<VideoPrepareResult> {
  return invoke<VideoPrepareResult>('prepare_video', { filePath, playerId, force });
}

export async function cancelVideoPrepare(playerId: string): Promise<void> {
  return invoke('cancel_video_prepare', { playerId });
}

export async function clearVideoCache(): Promise<number> {
  return invoke<number>('clear_video_cache');
}
