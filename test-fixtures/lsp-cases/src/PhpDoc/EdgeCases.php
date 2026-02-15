<?php

declare(strict_types=1);

namespace App\PhpDoc;

/**
 * CASE: Edge cases for phpDoc parser behavior.
 */
class EdgeCases
{
    /**
     * CASE: Multi-line summary parsing.
     * Second summary line should be appended.
     *
     * CASE: Unsupported tag should be ignored safely.
     * @template T
     *
     * CASE: ERROR-like malformed @param (missing $name) should be ignored.
     * @param string
     *
     * CASE: ERROR-like malformed @property (missing $name) should be ignored.
     * @property int
     *
     * CASE: ERROR-like malformed @method (missing parentheses) should be ignored.
     * @method string getName
     *
     * CASE: Bare @deprecated should produce default message "Deprecated".
     * @deprecated
     *
     * CASE: Nullable and intersection type parsing.
     * @param ?string $nickname
     * @return \ArrayAccess&\Countable
     * @throws \LogicException
     */
    public function parseRules(?string $nickname): \ArrayObject
    {
        if ($nickname === '') {
            throw new \LogicException('nickname cannot be empty string');
        }

        return new \ArrayObject([$nickname]);
    }

    public function localAnnotations(): void
    {
        /** CASE: Inline @var with intersection type. */
        /** @var \ArrayAccess&\Countable $collection */
        $collection = new \ArrayObject();

        /** CASE: Inline @var with nullable type. */
        /** @var ?string $alias */
        $alias = null;

        if ($alias === null) {
            $alias = 'fallback';
        }

        echo $collection->count() . $alias;
    }
}

