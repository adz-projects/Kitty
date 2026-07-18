import numpy as np

def build_dpp_kernel(embeddings, scores, diversity_weight=1.0):
    n = len(scores)
    if n == 0:
        return np.zeros((0, 0), dtype=np.float64)
    embs = np.asarray(embeddings, dtype=np.float64)
    norms = np.linalg.norm(embs, axis=1)
    norms[norms < 1e-12] = 1.0
    embs = embs / norms[:, np.newaxis]
    similarity = embs @ embs.T
    w = np.asarray(scores, dtype=np.float64) * diversity_weight
    W = np.diag(w)
    kernel = W @ similarity @ W
    return kernel

def dpp_sample(kernel, k, epsilon=1e-7):
    n = kernel.shape[0]
    if n == 0 or k <= 0:
        return []
    k = min(k, n)
    selected = []
    remaining = list(range(n))
    L = kernel.copy()
    for _ in range(k):
        diag = np.diag(L)[remaining]
        if np.all(diag <= 0):
            remaining_vals = {i: np.sum(np.abs(L[i, remaining]))
                            for i in remaining}
            best = max(remaining_vals, key=remaining_vals.get)
            selected.append(best)
            remaining.remove(best)
            continue
        best_local = remaining[int(np.argmax(diag))]
        selected.append(best_local)
        remaining.remove(best_local)
        if remaining:
            idx = remaining
            L_sel = L[best_local, :][idx]
            L_sub = L[np.ix_(idx, idx)]
            L_ss = max(L[best_local, best_local], epsilon)
            rank1 = np.outer(L_sel, L_sel) / L_ss
            L[np.ix_(idx, idx)] = L_sub - rank1
    return selected
