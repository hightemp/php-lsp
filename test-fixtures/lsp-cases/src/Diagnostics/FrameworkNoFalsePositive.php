<?php

namespace Symfony\Bundle\FrameworkBundle\Controller;

abstract class AbstractController
{
}

namespace Illuminate\Database\Eloquent;

class Model
{
}

class Builder
{
}

namespace App\Models;

class User extends \Illuminate\Database\Eloquent\Model
{
}

namespace App\Controller;

use App\Models\User;
use Symfony\Bundle\FrameworkBundle\Controller\AbstractController;

final class DashboardController extends AbstractController
{
    public function index(User $user): void
    {
        $this->render('dashboard.html.twig');
        $this->json(['ok' => true]);
        $this->redirectToRoute('dashboard');

        echo $user->email;
        User::whereEmail('demo@example.com')->firstOrFail();
    }
}
