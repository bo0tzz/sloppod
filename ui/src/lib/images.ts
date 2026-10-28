/**
 * Images attached to a prompt, as the `image` content blocks they're sent as. They go into the session's event log
 * and the agent's context, so each is scaled down and re-encoded until it fits the model's limits.
 */

export type Attachment = { mimeType: string; data: string };

/** The longest side the model looks at; a larger image only costs more. */
const MAX_SIDE = 1568;
/** The API takes images up to 5 MB once base64-encoded. */
export const MAX_BYTES = 3_500_000;
export const MAX_IMAGES = 10;
const SENT_AS_IS = ['image/png', 'image/jpeg', 'image/gif', 'image/webp'];

function base64(bytes: ArrayBuffer): string {
  let binary = '';
  const view = new Uint8Array(bytes);
  for (let i = 0; i < view.length; i += 0x8000) binary += String.fromCharCode(...view.subarray(i, i + 0x8000));
  return btoa(binary);
}

async function encode(blob: Blob): Promise<Attachment> {
  return { mimeType: blob.type, data: base64(await blob.arrayBuffer()) };
}

/** An image file as an attachment: as it is when the model takes it so, otherwise scaled down and re-encoded. */
export async function attach(file: File): Promise<Attachment> {
  if (!file.type.startsWith('image/')) throw new Error(`${file.name || 'That file'} isn't an image`);
  const bitmap = await createImageBitmap(file).catch(() => {
    throw new Error(`${file.name || 'That image'} couldn't be read`);
  });
  const scale = Math.min(1, MAX_SIDE / Math.max(bitmap.width, bitmap.height));
  if (scale === 1 && SENT_AS_IS.includes(file.type) && file.size <= MAX_BYTES) {
    bitmap.close();
    return encode(file);
  }
  const canvas = document.createElement('canvas');
  canvas.width = Math.max(1, Math.round(bitmap.width * scale));
  canvas.height = Math.max(1, Math.round(bitmap.height * scale));
  canvas.getContext('2d')?.drawImage(bitmap, 0, 0, canvas.width, canvas.height);
  bitmap.close();
  const toBlob = (type: string, quality?: number) =>
    new Promise<Blob | null>((resolve) => canvas.toBlob(resolve, type, quality));
  // PNG keeps screenshots' text sharp; a photo that's too big as one fits as a JPEG.
  for (const [type, quality] of [['image/png'], ['image/jpeg', 0.85], ['image/jpeg', 0.6]] as const) {
    const blob = await toBlob(type, quality);
    if (blob && blob.size <= MAX_BYTES) return encode(blob);
  }
  throw new Error(`${file.name || 'That image'} is too big to send`);
}

export const attachmentSrc = (image: Attachment) => `data:${image.mimeType};base64,${image.data}`;
