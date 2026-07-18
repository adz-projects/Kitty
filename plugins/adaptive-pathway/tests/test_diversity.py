import numpy as np
from src.adaptive_pathway.decision.diversity import build_dpp_kernel, dpp_sample


def test_build_kernel_empty():
    kernel = build_dpp_kernel([], [], diversity_weight=1.0)
    assert kernel.shape == (0, 0)


def test_build_kernel_shape():
    embeddings = [np.random.randn(384).astype(np.float64) for _ in range(5)]
    scores = [0.5, 0.7, 0.3, 0.9, 0.6]
    kernel = build_dpp_kernel(embeddings, scores)
    assert kernel.shape == (5, 5)


def test_build_kernel_symmetric():
    embeddings = [np.random.randn(384).astype(np.float64) for _ in range(4)]
    scores = [0.6, 0.8, 0.4, 0.7]
    kernel = build_dpp_kernel(embeddings, scores)
    assert np.allclose(kernel, kernel.T, atol=1e-6)


def test_build_kernel_positive_semidefinite():
    embeddings = [np.random.randn(384).astype(np.float64) for _ in range(6)]
    scores = [0.5 + 0.1 * i for i in range(6)]
    kernel = build_dpp_kernel(embeddings, scores)
    eigenvalues = np.linalg.eigvalsh(kernel)
    assert np.all(eigenvalues >= -1e-6)


def test_dpp_sample_empty_kernel():
    kernel = np.zeros((0, 0), dtype=np.float64)
    selected = dpp_sample(kernel, 3)
    assert selected == []


def test_dpp_sample_returns_unique():
    embeddings = [np.random.randn(384).astype(np.float64) for _ in range(10)]
    scores = [0.5 + 0.05 * i for i in range(10)]
    kernel = build_dpp_kernel(embeddings, scores)
    selected = dpp_sample(kernel, 5)
    assert len(selected) == 5
    assert len(set(selected)) == 5
    assert all(0 <= i < 10 for i in selected)


def test_dpp_sample_k_larger_than_n():
    embeddings = [np.random.randn(384).astype(np.float64) for _ in range(3)]
    scores = [0.5, 0.7, 0.9]
    kernel = build_dpp_kernel(embeddings, scores)
    selected = dpp_sample(kernel, 10)
    assert len(selected) == 3


def test_dpp_sample_k_zero():
    embeddings = [np.random.randn(384).astype(np.float64) for _ in range(5)]
    scores = [0.5] * 5
    kernel = build_dpp_kernel(embeddings, scores)
    selected = dpp_sample(kernel, 0)
    assert selected == []


def test_diversity_weight_effect():
    embeddings = [np.random.randn(384).astype(np.float64) for _ in range(8)]
    scores = [0.5] * 8
    kernel_low = build_dpp_kernel(embeddings, scores, diversity_weight=0.1)
    kernel_high = build_dpp_kernel(embeddings, scores, diversity_weight=2.0)

    diag_low = np.diag(kernel_low)
    diag_high = np.diag(kernel_high)
    assert np.sum(diag_high) > np.sum(diag_low) * 1.5


def test_kernel_numerical_stability():
    embeddings = [np.zeros(384, dtype=np.float64) for _ in range(3)]
    kernels = []
    for i in range(3):
        kernels.append(np.array(embeddings[i]))
    kernel = build_dpp_kernel(kernels, [0.5, 0.5, 0.5])
    assert not np.any(np.isnan(kernel))
    assert not np.any(np.isinf(kernel))


def test_dpp_sample_deterministic():
    np.random.seed(42)
    embeddings = [np.random.randn(384).astype(np.float64) for _ in range(5)]
    scores = [0.6, 0.7, 0.3, 0.8, 0.5]
    kernel = build_dpp_kernel(embeddings, scores)
    selected1 = dpp_sample(kernel, 3)
    selected2 = dpp_sample(kernel, 3)
    assert selected1 == selected2


def test_epsilon_stability():
    embeddings = [np.random.randn(384).astype(np.float64) for _ in range(4)]
    scores = [0.6, 0.6, 0.6, 0.6]
    kernel = build_dpp_kernel(embeddings, scores)
    selected = dpp_sample(kernel, 3, epsilon=1e-10)
    assert len(selected) == 3
