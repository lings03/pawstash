export function normalizePostId(postId: unknown): string {
  if (postId === null || postId === undefined) return '';
  if (typeof postId === 'string' || typeof postId === 'number') {
    const s = String(postId).trim();
    return s === '[object Object]' ? '' : s;
  }
  if (typeof postId === 'object') {
    const obj = postId as Record<string, unknown>;
    const candidate = obj.id ?? obj.post_id ?? obj.postId;
    if (candidate !== null && candidate !== undefined) {
      return normalizePostId(candidate);
    }
  }
  return '';
}

export function postCacheKey(
  service: string,
  creatorId: string | number,
  postId: unknown,
  providerId?: string
): string {
  const normService = String(service || '').trim().toLowerCase();
  const normCreator = String(creatorId || '').trim().toLowerCase();
  const normId = normalizePostId(postId);
  const prov = providerId && providerId !== 'auto' ? `:${providerId.trim().toLowerCase()}` : '';
  return `${normService}:${normCreator}:${normId}${prov}`;
}

export function creatorCacheKey(
  service: string,
  creatorId: string | number,
  providerId?: string
): string {
  const normService = String(service || '').trim().toLowerCase();
  const normCreator = String(creatorId || '').trim().toLowerCase();
  const prov = providerId && providerId !== 'auto' ? `:${providerId.trim().toLowerCase()}` : '';
  return `${normService}:${normCreator}${prov}`;
}

export function thumbnailKey(
  mediaId?: string | null,
  post?: { service?: string; user?: string; creator_id?: string; id?: unknown } | null
): string {
  if (post?.service && (post.user || post.creator_id) && post?.id) {
    const normService = post.service.trim().toLowerCase();
    const normUser = (post.user || post.creator_id || '').trim().toLowerCase();
    const normId = normalizePostId(post.id);
    if (normService && normUser && normId) {
      const cleanMedia = (mediaId || '').trim();
      return cleanMedia ? `post:${normService}:${normUser}:${normId}:${cleanMedia}` : `post:${normService}:${normUser}:${normId}`;
    }
  }
  return (mediaId || '').trim();
}
