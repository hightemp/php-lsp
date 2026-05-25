<?php

namespace App\Diagnostics;

final class PromotedSelfDefaults
{
    public function __construct(
        public ?string $objectManager = null,
        public ?array $mapping = null,
    ) {
    }

    public function withDefaults(self $defaults): static
    {
        $clone = clone $this;
        $clone->objectManager ??= $defaults->objectManager;
        $clone->mapping ??= $defaults->mapping ?? [];

        return $clone;
    }
}
