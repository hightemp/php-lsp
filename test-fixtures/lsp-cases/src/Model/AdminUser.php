<?php

declare(strict_types=1);

namespace App\Model;

// CASE: Inheritance target for go-to-definition and class references.
class AdminUser extends User
{
    // CASE: Method override for method resolution and hover.
    public function getName(): string
    {
        return 'admin:' . parent::getName();
    }
}

